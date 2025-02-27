use std::collections::HashMap;
use std::ffi::OsString;
use std::os::raw::c_void;
use std::path::{self, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use async_stream::stream;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Sender;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use super::core_foundation::types::{
    dispatch_queue_t, CFIndex, CFMutableArrayRef, FSEventStreamEventFlags, FSEventStreamRef,
};
use super::core_foundation::{self as CoreFoundation, types as CFTypes};
use super::KanshiOptions;
use crate::platforms::darwin::core_foundation::types::{
    kCFNumberSInt64Type, kFSEventStreamEventExtendedDataPathKey,
    kFSEventStreamEventExtendedFileIDKey,
};
use crate::platforms::darwin::core_foundation::{CFArrayGetValueAtIndex, CFDictionaryGetValue};
use crate::{
    FileSystemEvent, FileSystemEventType, FileSystemTarget, FileSystemTargetKind, KanshiError,
    KanshiImpl,
};

#[derive(Clone)]
pub struct FSEventsTracer {
    stream: Arc<RwLock<Option<WrappedEventStreamRef>>>,
    dispatch_queue: Arc<RwLock<Option<WrappedDispatchQueue>>>,
    sender: tokio::sync::broadcast::Sender<FileSystemEvent>,
    cancellation_token: CancellationToken,
    paths_to_watch: Arc<Mutex<Vec<PathBuf>>>,
}

pub struct WrappedEventStreamRef(FSEventStreamRef);
unsafe impl Send for WrappedEventStreamRef {}
unsafe impl Sync for WrappedEventStreamRef {}

pub struct WrappedDispatchQueue(dispatch_queue_t);
unsafe impl Send for WrappedDispatchQueue {}
unsafe impl Sync for WrappedDispatchQueue {}

extern "C" fn callback(
    _stream_ref: *const CFTypes::FSEventStreamRef, // ConstFSEventStreamRef - Reference to the stream this event originated from
    info: CFTypes::CFRef, // *mut FSEventStreamContext->info - Optionally supplied context during stream creation.
    num_event: usize,     // numEvents - Number of total events in this callback
    event_paths: CFTypes::CFRef, // eventPaths - Array of C Strings representing the paths where each event occurred
    event_flags: *const CFTypes::FSEventStreamEventFlags, // eventFlags - Array of EventFlags corresponding to each event
    _event_ids: *const CFTypes::FSEventStreamId, // eventIds - Array of EventIds corresponding to each event. This Id is guaranteed to always be increasing.
) {
    let sender = info as *const Sender<FileSystemEvent>;
    let mut inode_map = HashMap::<i64, FileSystemEvent>::new();
    for idx in 0..num_event {
        let dict = unsafe { CFArrayGetValueAtIndex(event_paths, idx as CFIndex) };
        let path = unsafe {
            CoreFoundation::cfstr_to_str(
                CoreFoundation::CFDictionaryGetValue(dict, *kFSEventStreamEventExtendedDataPathKey)
                    .cast(),
            )
        };

        let inode = unsafe {
            let mut value: i64 = 0;
            let ok = CoreFoundation::CFNumberGetValue(
                CFDictionaryGetValue(dict, *kFSEventStreamEventExtendedFileIDKey),
                kCFNumberSInt64Type,
                &mut value as *mut i64 as *mut CFTypes::CFRef,
            );
            if ok {
                Some(value)
            } else {
                None
            }
        };

        let flag = unsafe { *event_flags.add(idx) };

        let kind = if flag.contains(FSEventStreamEventFlags::kFSEventStreamEventFlagItemIsDir) {
            FileSystemTargetKind::Directory
        } else {
            FileSystemTargetKind::File
        };

        let mut event_type = match flag {
            x if x.contains(FSEventStreamEventFlags::kFSEventStreamEventFlagItemCreated) => {
                if x.contains(FSEventStreamEventFlags::kFSEventStreamEventFlagItemRemoved) {
                    FileSystemEventType::Delete
                } else if x.contains(FSEventStreamEventFlags::kFSEventStreamEventFlagItemRenamed) {
                    FileSystemEventType::Move
                } else {
                    FileSystemEventType::Create
                }
            }
            x if x.contains(FSEventStreamEventFlags::kFSEventStreamEventFlagItemRemoved) => {
                FileSystemEventType::Delete
            }
            x if x.contains(FSEventStreamEventFlags::kFSEventStreamEventFlagItemModified) => {
                FileSystemEventType::Modify
            }
            x if x.contains(FSEventStreamEventFlags::kFSEventStreamEventFlagItemRenamed) => {
                FileSystemEventType::Move
            }
            x => {
                eprintln!("Unknown Mask Received - {:?}", x);
                FileSystemEventType::Unknown
            }
        };

        if event_type == FileSystemEventType::Move && inode.is_some() {
            let inode = inode.unwrap();
            if inode_map.contains_key(&inode) {
                let mut old_event = inode_map.remove(&inode).unwrap();
                old_event.event_type = FileSystemEventType::MovedTo(OsString::from(path.clone()));
                event_type =
                    FileSystemEventType::MovedFrom(old_event.target.as_ref().unwrap().path.clone());

                let event = FileSystemEvent {
                    event_type,
                    target: Some(FileSystemTarget {
                        kind,
                        path: OsString::from(path),
                    }),
                };

                if let Err(e) = unsafe { (*sender).send(old_event) } {
                    eprintln!("Send Error Occurred - {:?}", e.to_string());
                }

                if let Err(e) = unsafe { (*sender).send(event) } {
                    eprintln!("Send Error Occurred - {:?}", e.to_string());
                }
            } else {
                // event_type =
                let event = FileSystemEvent {
                    event_type,
                    target: Some(FileSystemTarget {
                        kind,
                        path: OsString::from(path),
                    }),
                };

                inode_map.insert(inode, event);
            }
        } else {
            let event = FileSystemEvent {
                event_type,
                target: Some(FileSystemTarget {
                    kind,
                    path: OsString::from(path),
                }),
            };

            if let Err(e) = unsafe { (*sender).send(event) } {
                eprintln!("Send Error Occurred - {:?}", e.to_string());
            }
        }
    }
}

impl KanshiImpl<KanshiOptions> for FSEventsTracer {
    fn new(_opts: KanshiOptions) -> Result<FSEventsTracer, KanshiError> {
        let (tx, _rx) = tokio::sync::broadcast::channel(32);

        Ok(FSEventsTracer {
            stream: Arc::new(RwLock::new(None)),
            sender: tx,
            cancellation_token: CancellationToken::new(),
            paths_to_watch: Arc::new(Mutex::new(Vec::new())),
            dispatch_queue: Arc::new(RwLock::new(None)),
        })
    }

    async fn watch(&self, dir: &str) -> Result<(), KanshiError> {
        if let Some(_) = *self.stream.read().await {
            return Err(KanshiError::ListenerStartedError);
        }

        let mut paths_to_watch = self.paths_to_watch.lock().await;
        let path = path::absolute(Path::new(dir));
        if let Ok(path) = path {
            if !path.exists() {
                Err(KanshiError::FileSystemError(
                    "ENOENT Directory does not exist".to_owned(),
                ))
            } else {
                paths_to_watch.push(path);
                Ok(())
            }
        } else {
            Err(KanshiError::FileSystemError(
                path.err().unwrap().to_string(),
            ))
        }
    }

    fn get_events_stream(&self) -> Pin<Box<dyn futures::Stream<Item = FileSystemEvent> + Send>> {
        let mut listener = self.sender.subscribe();
        let cancel_token = self.cancellation_token.clone();

        Box::pin(stream! {
            'outer: loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break 'outer;
                    }
                    val = listener.recv() => {
                        match val {
                            Ok(x) => {
                              yield x
                            },
                            Err(e) => {
                              match e {
                                RecvError::Closed => break 'outer,
                                _ => ()
                            }}
                        }
                    }
                }
            }
        })
    }

    async fn start(&self) -> Result<(), KanshiError> {
        if let Some(_) = *self.stream.read().await {
            return Err(KanshiError::ListenerStartedError);
        }

        {
            let paths_to_watch = self.paths_to_watch.lock().await;
            // let sender = self.sender.clone();
            let ptr: *const Sender<FileSystemEvent> = &self.sender;

            let context = CFTypes::FSEventStreamContext {
                version: 0 as *mut i64,
                copy_description: None,
                retain: None,
                release: None,
                info: ptr as *mut c_void,
            };

            // drop(ptr);

            let paths_to_watch = unsafe {
                let paths: CFMutableArrayRef = CoreFoundation::CFArrayCreateMutable(
                    CFTypes::kCFAllocatorDefault,
                    0 as CFIndex,
                    &CoreFoundation::kCFTypeArrayCallBacks,
                );

                for path in paths_to_watch.iter() {
                    if !path.exists() {
                        return Err(KanshiError::FileSystemError(format!(
                            "{:?} does not exist",
                            path
                        )));
                    }

                    let canon_path = path.canonicalize()?;
                    let path_as_str = canon_path.to_str().unwrap();
                    let err: CFTypes::CFErrorRef = std::ptr::null_mut();
                    let cf_path = CoreFoundation::rust_str_to_cf_string(path_as_str, err);
                    if cf_path.is_null() {
                        CoreFoundation::CFRelease(err as CFTypes::CFRef);
                        return Err(KanshiError::FileSystemError(format!(
                            "{:?} does not exist",
                            path
                        )));
                    } else {
                        CoreFoundation::CFArrayAppendValue(paths, cf_path);
                        CoreFoundation::CFRelease(cf_path);
                    }
                }

                Ok(paths)
            };

            if let Err(e) = paths_to_watch {
                return Err(e);
            }

            let paths_to_watch = paths_to_watch.ok().unwrap();

            let flags = CFTypes::FSEventStreamCreateFlags::kFSEventStreamCreateFlagFileEvents
                | CFTypes::FSEventStreamCreateFlags::kFSEventStreamCreateFlagNoDefer
                | CFTypes::FSEventStreamCreateFlags::kFSEventStreamCreateFlagUseExtendedData
                | CFTypes::FSEventStreamCreateFlags::kFSEventStreamCreateFlagUseCFTypes;

            let stream = unsafe {
                CoreFoundation::FSEventStreamCreate(
                    CFTypes::kCFAllocatorDefault,
                    callback,
                    &context,
                    paths_to_watch,
                    CFTypes::kFSEventStreamEventIdSinceNow,
                    0.0,
                    flags,
                )
            };

            let dispatch_queue = unsafe {
                CoreFoundation::dispatch_queue_create(
                    std::ptr::null(),
                    CFTypes::DISPATCH_QUEUE_SERIAL,
                )
            };

            unsafe { CoreFoundation::FSEventStreamSetDispatchQueue(stream, dispatch_queue) };
            unsafe { CoreFoundation::FSEventStreamStart(stream) };

            if let Ok(mut stream_ref) = self.stream.try_write() {
                *stream_ref = Some(WrappedEventStreamRef(stream));
            }

            if let Ok(mut dq_ref) = self.dispatch_queue.try_write() {
                *dq_ref = Some(WrappedDispatchQueue(dispatch_queue));
            }
        }

        self.cancellation_token.cancelled().await;

        // Free the DispatchQueue
        // unsafe { dispatch_release(dispatch_queue) };

        Ok(())
    }

    fn close(&self) -> bool {
        if self.cancellation_token.is_cancelled() {
            return true;
        }

        self.cancellation_token.cancel();

        let mut has_errored = false;

        let stream_ref = self.stream.try_read();
        if let Ok(stream) = stream_ref {
            if stream.is_some() {
                let stream = stream.as_ref().unwrap();
                unsafe {
                    CoreFoundation::FSEventStreamStop(stream.0);
                    CoreFoundation::FSEventStreamInvalidate(stream.0);
                    CoreFoundation::FSEventStreamRelease(stream.0);
                };
            }
        } else {
            let e = stream_ref.err().unwrap();
            eprintln!("error occurred releasing stream {e}");
            has_errored = true;
        }

        let dq_ref = self.dispatch_queue.try_read();
        if let Ok(dq) = dq_ref {
            if dq.is_some() {
                let dq = dq.as_ref().unwrap();
                unsafe {
                    CoreFoundation::dispatch_release(dq.0);
                };
            }
        } else {
            let e = dq_ref.err().unwrap();
            eprintln!("error occurred releasing stream {e}");
            has_errored = true;
        }

        !has_errored
    }
}
