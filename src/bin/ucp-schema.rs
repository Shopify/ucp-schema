//! UCP Schema CLI
//!
//! Command-line interface for resolving and validating UCP schemas.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ucp_schema::{
    bundle_refs, compose_from_payload, detect_direction, load_schema, load_schema_auto, resolve,
    validate, DetectedDirection, Direction, ResolveOptions, ValidateError,
};

#[derive(Parser)]
#[command(name = "ucp-schema")]
#[command(about = "Resolve and validate UCP schema annotations")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Resolve a schema for a specific direction and operation
    Resolve {
        /// Schema source: file path or URL (http:// or https://)
        schema: String,

        /// Resolve for request direction
        #[arg(
            long,
            conflicts_with = "response",
            required_unless_present = "response"
        )]
        request: bool,

        /// Resolve for response direction
        #[arg(long, conflicts_with = "request", required_unless_present = "request")]
        response: bool,

        /// Operation to resolve for (e.g., create, update, read)
        #[arg(long, short)]
        op: String,

        /// Output file (stdout if not specified)
        #[arg(long)]
        output: Option<PathBuf>,

        /// Pretty-print JSON output
        #[arg(long)]
        pretty: bool,

        /// Dereference all $ref pointers (bundle into single schema)
        #[arg(long)]
        bundle: bool,

        /// Strict mode: set additionalProperties=false to reject unknown fields (default: false)
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        strict: bool,
    },

    /// Validate a payload against a resolved schema
    Validate {
        /// Payload file to validate
        payload: PathBuf,

        /// Explicit schema (default: infer from payload's UCP metadata)
        #[arg(long)]
        schema: Option<String>,

        /// Local directory for schema URL resolution
        #[arg(long)]
        schema_base: Option<PathBuf>,

        /// Validate as request (auto-inferred if omitted)
        #[arg(long, conflicts_with = "response")]
        request: bool,

        /// Validate as response (auto-inferred if omitted)
        #[arg(long, conflicts_with = "request")]
        response: bool,

        /// Operation to validate for (e.g., create, update, read)
        #[arg(long, short)]
        op: String,

        /// Output results as JSON (for automation)
        #[arg(long)]
        json: bool,

        /// Strict mode: reject unknown fields (default: false)
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        strict: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Resolve {
            schema,
            request,
            response: _,
            op,
            output,
            pretty,
            bundle,
            strict,
        } => run_resolve(&schema, request, op, output, pretty, bundle, strict),

        Commands::Validate {
            payload,
            schema,
            schema_base,
            request,
            response,
            op,
            json,
            strict,
        } => run_validate(ValidateArgs {
            payload,
            schema,
            schema_base,
            request,
            response,
            op,
            json_output: json,
            strict,
        }),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

fn run_resolve(
    schema_source: &str,
    request: bool,
    op: String,
    output: Option<PathBuf>,
    pretty: bool,
    bundle: bool,
    strict: bool,
) -> Result<(), u8> {
    let direction = Direction::from_request_flag(request);

    let mut schema = load_schema_auto(schema_source).map_err(|e| {
        eprintln!("Error: {}", e);
        e.exit_code() as u8
    })?;

    // Bundle: dereference all $refs before resolving annotations
    if bundle {
        // Resolve external file refs and their internal refs using our loader
        // Note: $ref: "#" (self-refs) are left as-is since they're recursive
        let base_dir = std::path::Path::new(schema_source)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        bundle_refs(&mut schema, base_dir).map_err(|e| {
            eprintln!("Error bundling refs: {}", e);
            e.exit_code() as u8
        })?;
    }

    let options = ResolveOptions::new(direction, op).strict(strict);
    let resolved = resolve(&schema, &options).map_err(|e| {
        eprintln!("Error: {}", e);
        e.exit_code() as u8
    })?;

    let json_output = if pretty {
        serde_json::to_string_pretty(&resolved)
    } else {
        serde_json::to_string(&resolved)
    }
    .map_err(|e| {
        eprintln!("Error serializing output: {}", e);
        2u8
    })?;

    match output {
        Some(path) => {
            std::fs::write(&path, &json_output).map_err(|e| {
                eprintln!("Error writing to {}: {}", path.display(), e);
                3u8
            })?;
        }
        None => {
            println!("{}", json_output);
        }
    }

    Ok(())
}

struct ValidateArgs {
    payload: PathBuf,
    schema: Option<String>,
    schema_base: Option<PathBuf>,
    request: bool,
    response: bool,
    op: String,
    json_output: bool,
    strict: bool,
}

fn run_validate(args: ValidateArgs) -> Result<(), u8> {
    let ValidateArgs {
        payload: payload_path,
        schema: schema_source,
        schema_base,
        request,
        response,
        op,
        json_output,
        strict,
    } = args;
    // Load payload first - needed for both explicit and self-describing modes
    let payload = load_schema(&payload_path).map_err(|e| {
        report_error(json_output, &format!("loading payload: {}", e));
        e.exit_code() as u8
    })?;

    // Determine direction: explicit flag > auto-inference from payload
    let direction = if request {
        Direction::Request
    } else if response {
        Direction::Response
    } else {
        // Auto-infer from payload structure
        match detect_direction(&payload) {
            Some(DetectedDirection::Response) => Direction::Response,
            Some(DetectedDirection::Request) => Direction::Request,
            None => {
                // No UCP metadata found - require explicit flag
                report_error(json_output, "cannot infer direction: payload has no ucp.capabilities or ucp.meta.profile. Use --request or --response.");
                return Err(2);
            }
        }
    };

    // Get schema: explicit --schema or compose from payload metadata
    let schema = match &schema_source {
        Some(source) => {
            // Explicit schema - load directly
            load_schema_auto(source).map_err(|e| {
                report_error(json_output, &format!("loading schema: {}", e));
                e.exit_code() as u8
            })?
        }
        None => {
            // Self-describing mode - compose from payload's UCP metadata
            let base = schema_base.as_deref();
            compose_from_payload(&payload, base).map_err(|e| {
                report_error(json_output, &e.to_string());
                e.exit_code() as u8
            })?
        }
    };

    let options = ResolveOptions::new(direction, op).strict(strict);

    match validate(&schema, &payload, &options) {
        Ok(()) => {
            if json_output {
                println!(r#"{{"valid":true}}"#);
            } else {
                println!("Valid");
            }
            Ok(())
        }
        Err(ValidateError::Invalid { errors, .. }) => {
            if json_output {
                let output = serde_json::json!({
                    "valid": false,
                    "errors": errors
                });
                println!("{}", output);
            } else {
                eprintln!("Validation failed:");
                for error in errors {
                    eprintln!("  {}", error);
                }
            }
            Err(1)
        }
        Err(ValidateError::Resolve(e)) => {
            report_error(json_output, &e.to_string());
            Err(e.exit_code() as u8)
        }
    }
}

/// Output an error message in plain text or JSON format.
fn report_error(json_output: bool, msg: &str) {
    if json_output {
        println!(r#"{{"valid":false,"error":"{}"}}"#, msg);
    } else {
        eprintln!("Error: {}", msg);
    }
}
