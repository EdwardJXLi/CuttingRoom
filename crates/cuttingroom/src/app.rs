use std::{
    cell::{Cell, RefCell},
    error::Error,
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    rc::Rc,
    sync::{Arc, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

use async_channel::{Receiver, Sender, bounded};
use cap_std::{ambient_authority, fs::Dir};
use cr_project::{OpenError, OpenOutcome, PhotographGroup, PhotographList, Project};
use slint::{ComponentHandle, Weak};

use crate::{
    MainWindow, Message, MessageSeverity,
    grid::{Grid, GridSession, ThumbnailInventory},
    interrupt,
    thumbnail_cache::ThumbnailCache,
};

const PHOTO_LIST_LIMIT: u32 = 10_000;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

enum ProjectEvent {
    Confirm(Message, Sender<bool>),
    Opened(String),
    OpenCancelled,
    OpenFailed(OpenError),
    ScanProgress(i64),
    ScanFinished(Result<ScannedProject, OpenError>),
    ThumbnailBatch(u64, Vec<PhotographGroup>),
    ThumbnailCacheCleared(io::Result<()>),
}

pub(crate) enum ProjectRequest {
    Rescan,
    ThumbnailBatch(u64),
    RebuildThumbnailCache(Arc<Dir>, Option<Arc<ThumbnailCache>>),
}

struct ScannedProject {
    listing: PhotographList,
    directory: Option<Dir>,
    thumbnails: ThumbnailInventory,
}

struct ProjectSession {
    events: Receiver<ProjectEvent>,
    requests: Sender<ProjectRequest>,
    task: slint::JoinHandle<()>,
    worker: thread::JoinHandle<()>,
}

pub(crate) fn run(initial_folder: Option<PathBuf>) -> Result<ExitCode, Box<dyn Error>> {
    let window = MainWindow::new()?;
    let interrupted = interrupt::install()?;

    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    let open_failed = Rc::new(Cell::new(false));
    let session = initial_folder
        .map(|folder| {
            window.set_status_text("Opening folder…".into());
            let (requests, requested) = bounded(2);
            let grid = GridSession::start(&window, requests.clone())?;
            match start_project(
                &window,
                folder,
                Rc::clone(&open_failed),
                grid.grid.clone(),
                requests,
                requested,
            ) {
                Ok(project) => Ok((grid, project)),
                Err(error) => {
                    grid.finish()?;
                    Err(error)
                }
            }
        })
        .transpose()?;

    let result = window.run();
    if let Some((grid, project)) = session {
        project.cancel(&window);
        let grid_result = grid.finish();
        project.finish()?;
        grid_result?;
    }
    result?;
    if interrupted.load(Ordering::Relaxed) {
        Ok(ExitCode::from(130))
    } else if open_failed.get() {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn start_project(
    window: &MainWindow,
    folder: PathBuf,
    open_failed: Rc<Cell<bool>>,
    grid: Rc<RefCell<Grid>>,
    requests: Sender<ProjectRequest>,
    requested: Receiver<ProjectRequest>,
) -> Result<ProjectSession, Box<dyn Error>> {
    let (sender, events) = bounded(1);
    let task = slint::spawn_local(handle_events(
        window.as_weak(),
        events.clone(),
        open_failed,
        grid.clone(),
    ))?;
    let worker = thread::Builder::new()
        .name("project worker".into())
        .spawn(move || run_project(&folder, sender, requested))?;

    let weak = window.as_weak();
    let rescan = requests.clone();
    window.on_rescan_folder(move || {
        if let Some(window) = weak.upgrade()
            && window.get_project_open()
            && !window.get_scan_active()
            && !window.get_thumbnail_reset_active()
            && rescan.try_send(ProjectRequest::Rescan).is_ok()
        {
            show_scan_started(&window);
        }
    });
    let weak = window.as_weak();
    window.on_rebuild_thumbnail_cache(move || {
        if let Some(window) = weak.upgrade()
            && window.get_project_open()
            && !window.get_scan_active()
            && !window.get_thumbnail_reset_active()
        {
            grid.borrow_mut().rebuild_thumbnail_cache(&window);
        }
    });
    Ok(ProjectSession {
        events,
        requests,
        task,
        worker,
    })
}

impl ProjectSession {
    fn cancel(&self, window: &MainWindow) {
        self.events.close();
        self.requests.close();
        window.on_message_dismissed(|_| {});
        // Dropping an undelivered confirmation also releases its waiting worker.
        drop(self.events.try_recv());
    }

    fn finish(self) -> io::Result<()> {
        self.task.abort();
        self.worker
            .join()
            .map_err(|_| io::Error::other("project worker failed"))
    }
}

async fn handle_events(
    window: Weak<MainWindow>,
    events: Receiver<ProjectEvent>,
    open_failed: Rc<Cell<bool>>,
    grid: Rc<RefCell<Grid>>,
) {
    while let Ok(event) = events.recv().await {
        let Some(window) = window.upgrade() else {
            break;
        };
        match event {
            ProjectEvent::Confirm(message, reply) => {
                window.on_message_dismissed(move |accepted| {
                    let _ = reply.try_send(accepted);
                    reply.close();
                });
                window.invoke_show_message(message);
            }
            ProjectEvent::Opened(name) => {
                window.on_message_dismissed(|_| {});
                window.set_project_title(name.into());
                window.set_project_open(true);
                show_scan_started(&window);
            }
            ProjectEvent::OpenCancelled => {
                let _ = window.hide();
            }
            ProjectEvent::OpenFailed(error) => {
                open_failed.set(true);
                window.set_status_text("".into());
                let weak = window.as_weak();
                window.on_message_dismissed(move |_| {
                    if let Some(window) = weak.upgrade() {
                        let _ = window.hide();
                    }
                });
                show_error(&window, "Could not open project", &error);
            }
            ProjectEvent::ScanProgress(found) => {
                let unit = if found == 1 { "file" } else { "files" };
                window.set_status_text(format!("Scanning folder… {found} {unit} found").into());
            }
            ProjectEvent::ScanFinished(result) => {
                window.set_scan_active(false);
                match result {
                    Ok(project) => {
                        window.set_status_text(listing_status(&project.listing).into());
                        grid.borrow_mut().replace(
                            &window,
                            project.directory,
                            project.listing.groups,
                            project.thumbnails,
                        );
                    }
                    Err(error) => {
                        window.set_status_text("Folder scan failed".into());
                        show_error(&window, "Could not scan folder", &error);
                        grid.borrow_mut().resume_after_scan_failure(&window);
                    }
                }
            }
            ProjectEvent::ThumbnailCacheCleared(result) => {
                window.set_thumbnail_reset_active(false);
                show_scan_started(&window);
                if let Err(error) = result {
                    show_error(
                        &window,
                        "Could not clear thumbnail cache",
                        &OpenError::Io(error),
                    );
                }
            }
            ProjectEvent::ThumbnailBatch(scan, groups) => {
                grid.borrow_mut()
                    .accept_thumbnail_batch(&window, scan, groups);
            }
        }
    }
}

fn show_scan_started(window: &MainWindow) {
    window.set_status_text("Scanning folder…".into());
    window.set_scan_active(true);
}

fn listing_status(listing: &PhotographList) -> String {
    match listing.groups.len() {
        0 => "No photos found".to_owned(),
        count if listing.has_more => format!("Showing first {count} photos"),
        1 => "1 photo".to_owned(),
        count => format!("{count} photos"),
    }
}

fn show_error(window: &MainWindow, heading: &str, error: &OpenError) {
    eprintln!("CuttingRoom: {heading}. {error}");
    window.invoke_show_message(Message {
        severity: MessageSeverity::Error,
        heading: heading.into(),
        text: error.to_string().into(),
        ..Message::default()
    });
}

fn run_project(folder: &Path, events: Sender<ProjectEvent>, requests: Receiver<ProjectRequest>) {
    let project = match open_project(folder, &events) {
        Ok(Some(project)) => project,
        Ok(None) => {
            let _ = events.send_blocking(ProjectEvent::OpenCancelled);
            return;
        }
        Err(error) => {
            let _ = events.send_blocking(ProjectEvent::OpenFailed(error));
            return;
        }
    };
    let root = project.root();
    let name = root
        .file_name()
        .unwrap_or(root.as_os_str())
        .to_string_lossy();
    if events
        .send_blocking(ProjectEvent::Opened(name.into_owned()))
        .is_err()
    {
        return;
    }

    let mut scan = 0_u64;
    let mut unlisted = None;
    'scan: while !events.is_closed() {
        let directory = Dir::open_ambient_dir(root, ambient_authority()).ok();
        let mut last_progress = Instant::now();
        let result = project.sync_photographs_for_thumbnails(
            PHOTO_LIST_LIMIT,
            || events.is_closed(),
            |found| {
                if last_progress.elapsed() >= PROGRESS_INTERVAL {
                    let _ = events.try_send(ProjectEvent::ScanProgress(found));
                    last_progress = Instant::now();
                }
            },
        );
        let result = result.map(|(listing, remaining)| {
            scan = scan.wrapping_add(1);
            let cache = directory
                .as_ref()
                .and_then(|dir| ThumbnailCache::open(dir).ok());
            let unlisted_total = remaining.as_ref().map_or(0, |groups| groups.total());
            let unlisted_unavailable = listing.has_more && remaining.is_none();
            unlisted = remaining.filter(|_| cache.is_some());
            ScannedProject {
                listing,
                directory,
                thumbnails: ThumbnailInventory {
                    scan,
                    cache,
                    unlisted_total,
                    unlisted_unavailable,
                },
            }
        });
        if events
            .send_blocking(ProjectEvent::ScanFinished(result))
            .is_err()
        {
            break;
        }
        while let Ok(request) = requests.recv_blocking() {
            if events.is_closed() {
                return;
            }
            match request {
                ProjectRequest::Rescan => continue 'scan,
                ProjectRequest::RebuildThumbnailCache(directory, cache) => {
                    let cancelled = || events.is_closed();
                    let result = if let Some(cache) = cache {
                        cache.clear(&directory, cancelled)
                    } else {
                        ThumbnailCache::open(&directory)
                            .and_then(|cache| cache.clear(&directory, cancelled))
                    };
                    if events
                        .send_blocking(ProjectEvent::ThumbnailCacheCleared(result))
                        .is_err()
                    {
                        return;
                    }
                    continue 'scan;
                }
                ProjectRequest::ThumbnailBatch(id) if id == scan => {
                    let groups = unlisted
                        .as_mut()
                        .and_then(|cursor| cursor.next_batch().ok())
                        .unwrap_or_default();
                    if groups.is_empty()
                        || unlisted.as_ref().is_some_and(|cursor| cursor.is_finished())
                    {
                        unlisted = None;
                    }
                    if events
                        .send_blocking(ProjectEvent::ThumbnailBatch(id, groups))
                        .is_err()
                    {
                        return;
                    }
                }
                ProjectRequest::ThumbnailBatch(_) => (),
            }
        }
        break;
    }
}

fn open_project(
    folder: &Path,
    events: &Sender<ProjectEvent>,
) -> Result<Option<Project>, OpenError> {
    let outcome = Project::open(folder, || {
        Ok(confirm(
            events,
            Message {
                heading: "Open locked project?".into(),
                text: "Another CuttingRoom session may be editing this project. Continuing can overwrite changes and cause data loss.".into(),
                accept_label: "Continue anyway".into(),
                cancel_primary: true,
                ..Message::default()
            },
        ))
    })?;
    match outcome {
        OpenOutcome::Ready(project) => Ok(Some(project)),
        OpenOutcome::NeedsUpgrade(upgrade) => {
            let (from, to) = upgrade.versions();
            if confirm(
                events,
                Message {
                    heading: "Upgrade project?".into(),
                    text: format!(
                        "This project uses version {from}. Upgrade to version {to} to open it. CuttingRoom will keep a backup."
                    )
                    .into(),
                    accept_label: "Upgrade".into(),
                    ..Message::default()
                },
            ) {
                upgrade.apply().map(Some)
            } else {
                Ok(None)
            }
        }
        OpenOutcome::Cancelled => Ok(None),
    }
}

fn confirm(events: &Sender<ProjectEvent>, message: Message) -> bool {
    let message = Message {
        severity: MessageSeverity::Warning,
        cancellable: true,
        ..message
    };
    let (reply, answer) = bounded(1);
    events
        .send_blocking(ProjectEvent::Confirm(message, reply))
        .is_ok()
        && answer.recv_blocking().unwrap_or(false)
}
