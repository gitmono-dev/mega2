//! Hidden debug utilities for integration tests and operational diagnostics.
//!
//! These commands are intentionally not shown in top-level help and are not
//! part of the public CLI contract.

use clap::{Arg, ArgMatches, Command};
use futures::StreamExt;

use crate::{
    commands::{CommandContext, require_config},
    common::errors::{MegaError, MegaResult},
    context::AppContext,
    orbit_api::object_storage::{ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace},
};

pub fn cli() -> Command {
    Command::new("debug")
        .about("Hidden debugging utilities")
        .hide(true)
        .subcommand(
            Command::new("storage-smoke")
                .about("Smoke-test the configured object storage backend")
                .arg(
                    Arg::new("key")
                        .long("key")
                        .short('k')
                        .help("Object key to use for the smoke test")
                        .default_value("debug/storage-smoke/test-object.bin"),
                ),
        )
}

#[tokio::main]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let config = require_config(ctx, "debug")?;
    let (cmd, subcommand_args) = match args.subcommand() {
        Some((cmd, args)) => (cmd, args),
        _ => return Ok(()),
    };

    match cmd {
        "storage-smoke" => storage_smoke(config, subcommand_args).await,
        _ => Err(MegaError::Other(format!("Unknown debug subcommand: {cmd}"))),
    }
}

async fn storage_smoke(config: crate::config::Config, args: &ArgMatches) -> MegaResult {
    let key = args
        .get_one::<String>("key")
        .cloned()
        .unwrap_or_else(|| "debug/storage-smoke/test-object.bin".to_string());

    let context = AppContext::new(config).await?;
    let obj_storage = &context.storage.git_service.obj_storage;

    let object_key = ObjectKey {
        namespace: ObjectNamespace::Attachment,
        key: key.clone(),
    };

    let payload = b"monoengine storage smoke test payload".to_vec();
    let payload_bytes = bytes::Bytes::from(payload.clone());
    let stream = Box::pin(futures::stream::once(async move {
        Ok::<_, std::io::Error>(payload_bytes)
    }));

    obj_storage
        .inner
        .put_stream(&object_key, stream, ObjectMeta::default())
        .await
        .map_err(|e| MegaError::Other(format!("object storage put failed: {e}")))?;

    let (stream, _meta) = obj_storage
        .inner
        .get_stream(&object_key)
        .await
        .map_err(|e| MegaError::Other(format!("object storage get failed: {e}")))?;

    let got_bytes = read_object_stream(stream)
        .await
        .map_err(|e| MegaError::Other(format!("object storage read failed: {e}")))?;

    if got_bytes != payload {
        return Err(MegaError::Other(
            "object storage round-trip payload mismatch".to_string(),
        ));
    }

    obj_storage
        .inner
        .delete(&object_key)
        .await
        .map_err(|e| MegaError::Other(format!("object storage delete failed: {e}")))?;

    tracing::info!(key = %key, "object storage smoke test passed");
    Ok(())
}

async fn read_object_stream(mut stream: ObjectByteStream) -> Result<Vec<u8>, MegaError> {
    let mut buf = bytes::BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| MegaError::Other(format!("stream chunk error: {e}")))?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze().to_vec())
}
