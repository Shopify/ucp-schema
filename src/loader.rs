//! Schema loading from various sources.
//!
//! Handles loading schemas from files, strings, and HTTP URLs.

use std::path::Path;

use serde_json::Value;

use crate::error::ResolveError;

#[cfg(feature = "remote")]
use std::time::Duration;

/// Default timeout for HTTP requests (10 seconds).
#[cfg(feature = "remote")]
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Load a schema from a file path.
///
/// # Errors
///
/// Returns `ResolveError::FileNotFound` if the file doesn't exist,
/// or `ResolveError::InvalidJson` if the file isn't valid JSON.
pub fn load_schema(path: &Path) -> Result<Value, ResolveError> {
    if !path.exists() {
        return Err(ResolveError::FileNotFound {
            path: path.to_path_buf(),
        });
    }

    let content = std::fs::read_to_string(path).map_err(|source| ResolveError::ReadError {
        path: path.to_path_buf(),
        source,
    })?;

    serde_json::from_str(&content).map_err(|source| ResolveError::InvalidJson { source })
}

/// Load a schema from a JSON string.
///
/// # Errors
///
/// Returns `ResolveError::InvalidJson` if the string isn't valid JSON.
pub fn load_schema_str(content: &str) -> Result<Value, ResolveError> {
    serde_json::from_str(content).map_err(|source| ResolveError::InvalidJson { source })
}

/// Load a schema from an HTTP/HTTPS URL.
///
/// Requires the `remote` feature (enabled by default).
///
/// # Errors
///
/// Returns `ResolveError::NetworkError` if the request fails,
/// or `ResolveError::InvalidJson` if the response isn't valid JSON.
#[cfg(feature = "remote")]
pub fn load_schema_url(url: &str) -> Result<Value, ResolveError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|source| ResolveError::NetworkError {
            url: url.to_string(),
            source,
        })?;

    let response = client
        .get(url)
        .send()
        .map_err(|source| ResolveError::NetworkError {
            url: url.to_string(),
            source,
        })?;

    // Check for HTTP errors before parsing
    let response = response
        .error_for_status()
        .map_err(|source| ResolveError::NetworkError {
            url: url.to_string(),
            source,
        })?;

    response
        .json()
        .map_err(|source| ResolveError::NetworkError {
            url: url.to_string(),
            source,
        })
}

/// Check if a string looks like a URL (starts with http:// or https://).
pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Navigate a JSON Pointer fragment (e.g., "#/$defs/foo" or "#/properties/bar").
///
/// Returns the value at the given JSON Pointer path within the schema.
/// The fragment should start with '#' (e.g., "#/$defs/foo").
pub fn navigate_fragment(schema: &Value, fragment: &str) -> Result<Value, ResolveError> {
    // Remove leading # and split by /
    let path = fragment.trim_start_matches('#').trim_start_matches('/');
    if path.is_empty() {
        return Ok(schema.clone());
    }

    let mut current = schema;
    for part in path.split('/') {
        // Unescape JSON Pointer encoding (~1 = /, ~0 = ~)
        let key = part.replace("~1", "/").replace("~0", "~");
        current = current.get(&key).ok_or_else(|| ResolveError::BundleError {
            message: format!("fragment not found: {}", fragment),
        })?;
    }
    Ok(current.clone())
}

/// Recursively resolve and inline external $ref pointers.
///
/// Walks the schema tree, finds `$ref` values pointing to external files,
/// loads them, and replaces the $ref with the loaded content.
/// Internal refs (`#/...`) in the root schema are left for the validator.
/// Internal refs in loaded external files are resolved against that file.
/// Self-root refs (`$ref: "#"`) are left as-is (recursive type definitions).
///
/// # Arguments
/// * `schema` - The schema to process (modified in place)
/// * `base_dir` - Base directory for resolving relative file paths
pub fn bundle_refs(schema: &mut Value, base_dir: &Path) -> Result<(), ResolveError> {
    bundle_refs_inner(
        schema,
        base_dir,
        None,
        &mut std::collections::HashSet::new(),
    )
}

fn bundle_refs_inner(
    schema: &mut Value,
    base_dir: &Path,
    file_root: Option<&Value>, // Root of external file for resolving internal refs
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), ResolveError> {
    match schema {
        Value::Object(obj) => {
            // Check if this object has a $ref
            if let Some(ref_val) = obj.get("$ref").and_then(|v| v.as_str()) {
                if ref_val.starts_with('#') {
                    // Internal ref - only resolve if we have a file_root context
                    // Skip self-root refs ($ref: "#") - these are recursive type defs
                    if ref_val == "#" {
                        // Leave as-is - can't inline recursive self-reference
                    } else if let Some(root) = file_root {
                        let mut target = navigate_fragment(root, ref_val)?;
                        // Recursively process (may have nested refs)
                        bundle_refs_inner(&mut target, base_dir, file_root, visited)?;
                        // Inline the resolved definition
                        obj.remove("$ref");
                        if let Value::Object(ref_obj) = target {
                            for (k, v) in ref_obj {
                                obj.entry(k).or_insert(v);
                            }
                        }
                        return Ok(());
                    }
                    // No file_root = root schema, leave for validator
                } else {
                    // External ref - load the file
                    let (file_part, fragment) = match ref_val.find('#') {
                        Some(idx) => (&ref_val[..idx], Some(&ref_val[idx..])),
                        None => (ref_val, None),
                    };

                    let ref_path = base_dir.join(file_part);
                    let canonical = ref_path.canonicalize().unwrap_or(ref_path.clone());
                    let visit_key = format!("{}|{}", canonical.display(), fragment.unwrap_or(""));

                    if visited.contains(&visit_key) {
                        return Err(ResolveError::BundleError {
                            message: format!("circular reference detected: {}", ref_val),
                        });
                    }

                    // Load file - this becomes the new file_root for internal refs
                    let loaded = load_schema(&ref_path)?;
                    let mut target = if let Some(frag) = fragment {
                        navigate_fragment(&loaded, frag)?
                    } else {
                        loaded.clone()
                    };

                    visited.insert(visit_key.clone());
                    let ref_dir = ref_path.parent().unwrap_or(base_dir);
                    // Pass loaded file as file_root so internal refs resolve against it
                    bundle_refs_inner(&mut target, ref_dir, Some(&loaded), visited)?;
                    visited.remove(&visit_key);

                    obj.remove("$ref");
                    if let Value::Object(ref_obj) = target {
                        for (k, v) in ref_obj {
                            obj.entry(k).or_insert(v);
                        }
                    }
                    return Ok(());
                }
            }

            // Recurse into all values
            for value in obj.values_mut() {
                bundle_refs_inner(value, base_dir, file_root, visited)?;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                bundle_refs_inner(item, base_dir, file_root, visited)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Load a schema from a file path or URL.
///
/// Automatically detects whether the source is a URL or file path.
/// URL loading requires the `remote` feature.
///
/// # Errors
///
/// Returns appropriate errors based on the source type.
pub fn load_schema_auto(source: &str) -> Result<Value, ResolveError> {
    if is_url(source) {
        #[cfg(feature = "remote")]
        {
            load_schema_url(source)
        }
        #[cfg(not(feature = "remote"))]
        {
            Err(ResolveError::FileNotFound {
                path: std::path::PathBuf::from(source),
            })
        }
    } else {
        load_schema(Path::new(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_schema_valid_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type": "object"}}"#).unwrap();

        let schema = load_schema(file.path()).unwrap();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn load_schema_file_not_found() {
        let result = load_schema(Path::new("/nonexistent/path.json"));
        assert!(matches!(result, Err(ResolveError::FileNotFound { .. })));
    }

    #[test]
    fn load_schema_invalid_json() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "not valid json").unwrap();

        let result = load_schema(file.path());
        assert!(matches!(result, Err(ResolveError::InvalidJson { .. })));
    }

    #[test]
    fn load_schema_str_valid() {
        let schema = load_schema_str(r#"{"type": "object"}"#).unwrap();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn load_schema_str_invalid() {
        let result = load_schema_str("not json");
        assert!(matches!(result, Err(ResolveError::InvalidJson { .. })));
    }

    #[test]
    fn is_url_https() {
        assert!(is_url("https://example.com/schema.json"));
    }

    #[test]
    fn is_url_http() {
        assert!(is_url("http://example.com/schema.json"));
    }

    #[test]
    fn is_url_file_path() {
        assert!(!is_url("/path/to/schema.json"));
        assert!(!is_url("./schema.json"));
        assert!(!is_url("schema.json"));
    }

    #[test]
    fn load_schema_auto_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type": "string"}}"#).unwrap();

        let schema = load_schema_auto(file.path().to_str().unwrap()).unwrap();
        assert_eq!(schema["type"], "string");
    }

    // Remote tests - require network, use httpbin.org for reliable testing
    #[cfg(feature = "remote")]
    mod remote {
        use super::*;

        #[test]
        fn load_schema_url_valid() {
            // httpbin.org/json returns a well-known JSON response
            let result = load_schema_url("https://httpbin.org/json");
            assert!(result.is_ok());
            let schema = result.unwrap();
            // httpbin returns {"slideshow": {...}}
            assert!(schema.get("slideshow").is_some());
        }

        #[test]
        fn load_schema_url_404() {
            let result = load_schema_url("https://httpbin.org/status/404");
            assert!(matches!(result, Err(ResolveError::NetworkError { .. })));
        }

        #[test]
        fn load_schema_url_invalid_host() {
            let result =
                load_schema_url("https://this-domain-does-not-exist-12345.invalid/schema.json");
            assert!(matches!(result, Err(ResolveError::NetworkError { .. })));
        }

        #[test]
        fn load_schema_auto_url() {
            let result = load_schema_auto("https://httpbin.org/json");
            assert!(result.is_ok());
        }
    }
}
