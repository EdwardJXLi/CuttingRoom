use std::{
    cell::{Cell, RefCell},
    error::Error,
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use async_channel::{Receiver, Sender, bounded};
use cap_std::{ambient_authority, fs::Dir};
use cr_project::{OpenError, OpenOutcome, PhotographList, Project};
use slint::{ComponentHandle, Weak};

use crate::{
    MainWindow, Message, MessageSeverity,
    grid::{Grid, GridSession},
};

const FILE_LIST_LIMIT: u32 = 10_000;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

enum ProjectEvent {
    Confirm(Message, Sender<bool>),
    Opened(String),
    OpenCancelled,
    OpenFailed(OpenError),
    ScanProgress(i64),
    ScanFinished(Result<(PhotographList, Option<Dir>), OpenError>),
}

struct ProjectSession {
    events: Receiver<ProjectEvent>,
    rescans: Sender<()>,
    task: slint::JoinHandle<()>,
    worker: thread::JoinHandle<()>,
}

pub(crate) fn run(initial_folder: Option<PathBuf>) -> Result<ExitCode, Box<dyn Error>> {
    let window = MainWindow::new()?;
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    let open_failed = Rc::new(Cell::new(false));
    let session = initial_folder
        .map(|folder| {
            window.set_status_text("Opening folder…".into());
            let grid = GridSession::start(&window)?;
            match start_project(&window, folder, Rc::clone(&open_failed), grid.grid.clone()) {
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
        let grid_result = grid.finish();
        project.finish(&window)?;
        grid_result?;
    }
    result?;
    if open_failed.get() {
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
) -> Result<ProjectSession, Box<dyn Error>> {
    let (sender, events) = bounded(1);
    let (rescans, requests) = bounded(1);
    let task = slint::spawn_local(handle_events(
        window.as_weak(),
        events.clone(),
        open_failed,
        grid,
    ))?;
    let worker = thread::Builder::new()
        .name("project worker".into())
        .spawn(move || run_project(&folder, sender, requests))?;

    let weak = window.as_weak();
    let rescan = rescans.clone();
    window.on_rescan_folder(move || {
        if let Some(window) = weak.upgrade()
            && window.get_project_open()
            && !window.get_scan_active()
            && rescan.try_send(()).is_ok()
        {
            show_scan_started(&window);
        }
    });
    Ok(ProjectSession {
        events,
        rescans,
        task,
        worker,
    })
}

impl ProjectSession {
    fn finish(self, window: &MainWindow) -> io::Result<()> {
        self.events.close();
        self.rescans.close();
        window.on_message_dismissed(|_| {});
        // Dropping an undelivered confirmation also releases its waiting worker.
        drop(self.events.try_recv());
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
                window.set_status_text(
                    format!("Scanning folder… {} found", photo_count(found)).into(),
                );
            }
            ProjectEvent::ScanFinished(result) => {
                window.set_scan_active(false);
                match result {
                    Ok((photos, directory)) => {
                        window.set_status_text(listing_status(photos.total).into());
                        grid.borrow_mut().replace(&window, directory, photos.paths);
                    }
                    Err(error) => {
                        window.set_status_text("Folder scan failed".into());
                        show_error(&window, "Could not scan folder", &error);
                    }
                }
            }
        }
    }
}

fn show_scan_started(window: &MainWindow) {
    window.set_status_text("Scanning folder…".into());
    window.set_scan_active(true);
}

fn listing_status(total: i64) -> String {
    match total {
        0 => "No photos found".to_owned(),
        total if total > i64::from(FILE_LIST_LIMIT) => {
            format!("Showing {FILE_LIST_LIMIT} of {total} photos")
        }
        total => photo_count(total),
    }
}

fn photo_count(count: i64) -> String {
    if count == 1 {
        "1 photo".to_owned()
    } else {
        format!("{count} photos")
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

fn run_project(folder: &Path, events: Sender<ProjectEvent>, rescans: Receiver<()>) {
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

    while !events.is_closed() {
        let directory = Dir::open_ambient_dir(root, ambient_authority()).ok();
        let mut last_progress = Instant::now();
        let result = project.sync_photographs(
            FILE_LIST_LIMIT,
            || events.is_closed(),
            |found| {
                if last_progress.elapsed() >= PROGRESS_INTERVAL {
                    let _ = events.try_send(ProjectEvent::ScanProgress(found));
                    last_progress = Instant::now();
                }
            },
        );
        if events
            .send_blocking(ProjectEvent::ScanFinished(
                result.map(|photos| (photos, directory)),
            ))
            .is_err()
            || rescans.recv_blocking().is_err()
        {
            break;
        }
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
