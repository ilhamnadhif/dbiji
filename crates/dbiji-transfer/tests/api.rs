//! Compile-time checks of the public API: the types the application moves
//! into background tasks are `Send`, shared ones are `Sync`, and the async
//! entry points produce `Send` futures (so they can be `tokio::spawn`ed).

use dbiji_core::Session;
use dbiji_transfer::*;
use std::future::Future;
use std::sync::Arc;

fn send<T: Send>() {}
fn sync<T: Sync>() {}
fn send_future<F: Future + Send>(_: F) {}

#[test]
fn types_are_send_and_sync() {
    send::<Box<dyn RowSink + Send>>();
    send::<Box<dyn RowSource>>();
    send::<CursorSource>();
    send::<RowStoreSource>();
    send::<VecSource>();
    send::<CsvSource>();
    send::<XlsxSource>();
    send::<XmlSource>();
    send::<JsonSource>();
    send::<PeekableSource<CsvSource>>();
    send::<ExportTarget>();
    sync::<Progress>();
    send::<Arc<dyn SessionFactory>>();
    sync::<ExportJob>();
    sync::<ImportMapping>();
    sync::<MigrationJob>();
    sync::<MockConfig>();
}

/// Never called: it only has to compile.
#[allow(dead_code)]
fn futures_are_send(
    source: &'static mut dyn RowSource,
    sink: &'static mut dyn RowSink,
    session: &'static mut dyn Session,
    factory: Arc<dyn SessionFactory>,
    mapping: &'static ImportMapping,
    progress: &'static Progress,
    cancel: &'static CancellationToken,
) {
    send_future(run_export(source, sink, progress, cancel));
    send_future(run_export_job(ExportJob::default(), factory.clone(), Arc::new(Progress::new()), cancel.clone()));
    send_future(run_import_job(ImportJob::default(), factory.clone(), Arc::new(Progress::new()), cancel.clone()));
    send_future(run_migration(MigrationJob::default(), factory.clone(), factory, progress, cancel));
    let _ = (session, mapping);
}

#[allow(dead_code)]
fn import_future_is_send(
    source: &'static mut dyn RowSource,
    session: &'static mut dyn Session,
    mapping: &'static ImportMapping,
    progress: &'static Progress,
    cancel: &'static CancellationToken,
) {
    send_future(run_import(source, session, mapping, progress, cancel));
}

#[allow(dead_code)]
fn mock_future_is_send(session: &'static mut dyn Session, config: &'static MockConfig, progress: &'static Progress, cancel: &'static CancellationToken) {
    send_future(run_mock(config, session, progress, cancel));
}
