use std::{
    collections::{HashSet, VecDeque}, ffi::{OsStr, OsString}, fs, io, os::{
        fd::{AsFd, AsRawFd},
        unix::fs::MetadataExt,
    }, path::{Path, PathBuf}, pin::Pin, sync::Arc
};

use async_stream::stream;
use nix::{
    errno::Errno,
    fcntl::AT_FDCWD,
    sys::{
        epoll::Epoll,
        fanotify::{Fanotify, FanotifyFidEventInfoType, FanotifyFidRecord, FanotifyInfoRecord},
    },
};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::{
    FileSystemEvent, FileSystemEventType, FileSystemTarget, FileSystemTargetKind, KanshiError,
    KanshiImpl,
};

use super::KanshiOptions;

#[derive(Clone)]
pub struct FanotifyTracer {
    fanotify: Arc<Fanotify>,
    epoll: Arc<Epoll>,
    sender: tokio::sync::broadcast::Sender<FileSystemEvent>,
    cancellation_token: CancellationToken,
}

#[repr(C)]
#[derive(Debug)]
pub struct FileHandle {
    pub handle_bytes: u32,
    pub handle_type: i32,
    pub f_handle: [u8; 0],
}

impl KanshiImpl<KanshiOptions> for FanotifyTracer {
    fn new(_opts: KanshiOptions) -> Result<FanotifyTracer, KanshiError> {
        use nix::sys::epoll::{EpollCreateFlags, EpollEvent, EpollFlags};
        use nix::sys::fanotify::{EventFFlags, InitFlags};

        #[allow(non_snake_case)]
        let INIT_FLAGS: InitFlags = InitFlags::FAN_CLASS_NOTIF
            | InitFlags::FAN_REPORT_DFID_NAME
            | InitFlags::FAN_UNLIMITED_QUEUE
            // | InitFlags::FAN_REPORT_TARGET_FID
            // | InitFlags::FAN_REPORT_FID
            | InitFlags::FAN_UNLIMITED_MARKS;
        #[allow(non_snake_case)]
        let EVENT_FLAGS: EventFFlags =
            EventFFlags::O_RDONLY | EventFFlags::O_NONBLOCK | EventFFlags::O_CLOEXEC;

        let fanotify_fd = Fanotify::init(INIT_FLAGS, EVENT_FLAGS);

        if let Ok(fanotify) = fanotify_fd {
            // Setup epoll
            let epoll_event =
                EpollEvent::new(EpollFlags::EPOLLIN, fanotify.as_fd().as_raw_fd() as u64);

            let epoll_fd = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC);

            if let Ok(epoll) = epoll_fd {
                if let Err(e) = epoll.add(fanotify.as_fd(), epoll_event) {
                    Err(KanshiError::FileSystemError(e.to_string()))
                } else {
                    let (tx, _rx) = tokio::sync::broadcast::channel(32);
                    let engine = FanotifyTracer {
                        // mark_set: HashSet::new(),
                        fanotify: Arc::new(fanotify),
                        epoll: Arc::new(epoll),
                        sender: tx,
                        // reciever: rx,
                        cancellation_token: CancellationToken::new(),
                    };
                    Ok(engine)
                }
            } else {
                let e = epoll_fd.err().unwrap();
                Err(KanshiError::FileSystemError(e.to_string()))
            }
        } else {
            Err(KanshiError::FileSystemError(
                io::Error::last_os_error().to_string(),
            ))
        }
    }

    async fn watch(&self, dir: &str) -> Result<(), KanshiError> {
        if self.cancellation_token.is_cancelled() {
            return Err(KanshiError::StreamClosedError);
        }

        let mark_top_dir = mark(&self.fanotify, Path::new(dir));

        if let Ok(_) = mark_top_dir {
            let mut traversal_queue = VecDeque::from([PathBuf::from(dir)]);
            let mut visited = HashSet::<u64>::new();

            'outer: loop {
                if let Some(next_dir) = traversal_queue.pop_front() {
                    if let Ok(dir_items) = fs::read_dir(next_dir) {
                        for dir_item in dir_items {
                            if let Ok(dir_item_unwrapped) = dir_item {
                                if let Ok(metadata) = dir_item_unwrapped.metadata() {
                                    let inode_number = metadata.ino();
                                    if !visited.contains(&inode_number) && !metadata.is_symlink() {
                                        visited.insert(inode_number);
                                        if dir_item_unwrapped.path().is_dir() {
                                            if let Err(e) =
                                                mark(&self.fanotify, &dir_item_unwrapped.path())
                                            {
                                                return Err(e);
                                            }
                                            traversal_queue.push_back(dir_item_unwrapped.path());
                                        }
                                    }
                                }
                            } else {
                                break 'outer;
                            }
                        }
                    } else {
                        break 'outer;
                    }
                } else {
                    break 'outer;
                }
            }

            Ok(())
        } else {
            mark_top_dir
        }
    }

    fn get_events_stream(&self) -> Pin<Box<dyn futures::Stream<Item = FileSystemEvent> + Send>> {
        let mut listener = self.sender.subscribe();
        let cancel_token = self.cancellation_token.clone();

        let events_stream = stream! {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    val = listener.recv() => {
                        match val {
                            Ok(x) => yield x,
                            Err(e) => match e {
                                RecvError::Closed => break,
                                _ => ()
                            }
                        }
                    }
                }
            }
        };

        Box::pin(events_stream)
    }

    async fn start(&self) -> Result<(), KanshiError> {
        use nix::sys::epoll::EpollEvent;

        let cancel_token = self.cancellation_token.clone();
        let sender = self.sender.clone();

        let mut events = [EpollEvent::empty(); 1];

        while !cancel_token.is_cancelled() {
            use nix::sys::fanotify::MaskFlags;

            events.fill(EpollEvent::empty());
            let res = tokio::task::block_in_place(move || self.epoll.wait(&mut events, 16u8));
            if let Err(e) = res {
                println!("epoll failed {e}");
                res?;
            }
            if res.ok().unwrap() > 0 {
                let all_records = self.fanotify.read_events_with_info_records()?;
                'outer: for (event, records) in all_records {
                    let kind = if event.mask().contains(MaskFlags::FAN_ONDIR) {
                        FileSystemTargetKind::Directory
                    } else {
                        FileSystemTargetKind::File
                    };
                    // Handle Moves/Renames separately
                    if event.mask().contains(MaskFlags::FAN_RENAME) {
                        let mut moved_from = None;
                        let mut moved_to = None;
                        for record in records {
                            if let FanotifyInfoRecord::Fid(record) = record {
                                let path = {
                                    let path = get_path_from_record(&record);
                                    if let Err(e) = path {
                                        if e == Errno::ESTALE {
                                            break;
                                        }
                                        println!("another error occurred ${e}");
                                    }
                                    path?
                                };
                                if record.info_type() == FanotifyFidEventInfoType::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME {
                                    moved_from = Some(path);
                                } else if record.info_type() == FanotifyFidEventInfoType::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME {
                                    moved_to = Some(path);
                                }
                            }
                        }

                        if moved_from.is_none() || moved_to.is_none() {
                            let tracer_event = FileSystemEvent {
                                event_type: FileSystemEventType::Move,
                                target: Some(FileSystemTarget {
                                    path: moved_from.or(moved_to).unwrap_or(OsString::new()),
                                    kind,
                                }),
                            };
                            if let Err(_) = sender.send(tracer_event) {
                                return Err(KanshiError::StreamClosedError);
                            }
                        } else {
                            let tracer_event1 = FileSystemEvent {
                                event_type: FileSystemEventType::MovedTo(moved_to.clone().unwrap()),
                                target: Some(FileSystemTarget {
                                    path: moved_from.clone().unwrap(),
                                    kind: kind.clone(),
                                }),
                            };

                            let tracer_event2 = FileSystemEvent {
                                event_type: FileSystemEventType::MovedFrom(moved_from.unwrap()),
                                target: Some(FileSystemTarget {
                                    path: moved_to.clone().unwrap(),
                                    kind,
                                }),
                            };

                            if let Err(_) = sender.send(tracer_event1) {
                                return Err(KanshiError::StreamClosedError);
                            }

                            if let Err(_) = sender.send(tracer_event2) {
                                return Err(KanshiError::StreamClosedError);
                            }
                        }
                    } else {
                        let mut tracer_event = FileSystemEvent {
                            event_type: match event.mask() {
                                x if x.contains(MaskFlags::FAN_CREATE) => {
                                    FileSystemEventType::Create
                                }
                                x if x.contains(MaskFlags::FAN_DELETE_SELF) => {
                                    FileSystemEventType::Delete
                                }
                                x if x.contains(MaskFlags::FAN_DELETE) => {
                                    FileSystemEventType::Delete
                                }
                                x if x.contains(MaskFlags::FAN_MODIFY) => {
                                    FileSystemEventType::Modify
                                }
                                x if x.contains(MaskFlags::FAN_MOVE_SELF) => {
                                    FileSystemEventType::Move
                                }
                                x => {
                                    eprintln!("Unknown Mask Received - {:?}", x);
                                    FileSystemEventType::Unknown
                                }
                            },
                            target: None,
                        };
                        let mut path = None;
                        for record in records {
                            if let FanotifyInfoRecord::Fid(record) = record {
                                path = Some({
                                    let path = get_path_from_record(&record);
                                    if let Err(e) = path {
                                        if e == Errno::ESTALE {
                                            continue 'outer;
                                        }
                                        println!("another error occurred ${e}");
                                    }
                                    path?
                                });
                            }
                        }
                        if path.is_some() && path.as_ref().unwrap().len() > 0 {
                            if event.mask().contains(MaskFlags::FAN_CREATE)
                                && kind == FileSystemTargetKind::Directory
                            {
                                let path = Path::new(path.as_ref().unwrap());

                                // Add new directory to fanotify
                                if let Err(err) = mark(&self.fanotify, path) {
                                    // We ignore ENOENT errors as it likely means a file was immediately created and deleted
                                    if let KanshiError::FileSystemError(e) = err.clone() {
                                        if !e.contains("ENOENT") {
                                            return Err(err);
                                        }
                                    }
                                }
                            }
                            tracer_event.target = Some(FileSystemTarget {
                                kind: kind.clone(),
                                path: path.unwrap(),
                            });
                        }

                        if let Err(_) = sender.send(tracer_event) {
                            return Err(KanshiError::StreamClosedError);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn close(&self) -> bool {
        use nix::sys::fanotify::{MarkFlags, MaskFlags};

        if self.cancellation_token.is_cancelled() {
            return true;
        }

        self.cancellation_token.cancel();

        #[allow(non_snake_case)]
        let MARK_FLAGS = MarkFlags::FAN_MARK_FLUSH;

        let mut has_error = false;

        if self.epoll.delete(self.fanotify.as_fd()).is_err() {
            println!("epoll.delete returned error");
            has_error = true;
        }
        if self
            .fanotify
            .mark(MARK_FLAGS, MaskFlags::empty(), AT_FDCWD, Some("/"))
            .is_err()
        {
            println!("fanotify.mark returned error");
            has_error = true;
        }
        !has_error
    }
}

impl Drop for FanotifyTracer {
    fn drop(&mut self) {
        // println!("dropped!");
    }
}

fn mark(fanotify: &Fanotify, path: &Path) -> Result<(), KanshiError> {
    use nix::sys::fanotify::{MarkFlags, MaskFlags};
    #[allow(non_snake_case)]
    let MARK_FLAGS = MarkFlags::FAN_MARK_ADD;
    #[allow(non_snake_case)]
    let MASK_FLAGS = MaskFlags::FAN_ONDIR
        | MaskFlags::FAN_EVENT_ON_CHILD
        | MaskFlags::FAN_CREATE
        | MaskFlags::FAN_MODIFY
        | MaskFlags::FAN_DELETE
        | MaskFlags::FAN_RENAME;

    if let Err(e) = fanotify.mark(MARK_FLAGS, MASK_FLAGS, AT_FDCWD, Some(path)) {
        Err(KanshiError::FileSystemError(e.to_string()))
    } else {
        Ok(())
    }
}

fn get_path_from_record(record: &FanotifyFidRecord) -> Result<OsString, Errno> {
    let mut path = OsString::new();

    let handle = &record.handle();
    let fh = handle.as_ptr() as *mut FileHandle;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_open_by_handle_at,
            AT_FDCWD,
            fh,
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_PATH | libc::O_NONBLOCK,
        )
    };

    if fd > 0 {
        let fd_path = format!("/proc/self/fd/{fd}");
        path.push(nix::fcntl::readlink::<OsStr>(fd_path.as_ref())?);
        unsafe { libc::close(fd as i32) };
    } else {
        return Err(Errno::last());
    }

    let file_name = record.name();

    if let Some(name) = file_name {
        if name != "." {
            path.push("/");
            path.push(name);
        }
    }

    Ok(path)
}
