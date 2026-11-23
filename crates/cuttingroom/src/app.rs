use std::{
    cell::RefCell,
    error::Error,
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    rc::Rc,
    thread,
};

use async_channel::{Receiver, Sender, bounded};
use cr_project::{OpenError, OpenOutcome, PhotographList, Project};
use slint::{ComponentHandle, ModelRc, VecModel, Weak, language::StandardListViewItem};

use crate::{MainWindow, Message, MessageSeverity};

const FILE_LIST_LIMIT: u32 = 10_000;

struct LoadedProject {
    project: Project,
    photographs: PhotographList,
}

enum ProjectEvent {
    Confirm(Message, Sender<bool>),
    Finished(Result<Option<LoadedProject>, OpenError>),
}

#[derive(Default)]
struct WindowState {
    project: Option<Project>,
    initial_open_failed: bool,
}

struct ProjectOpen {
    requests: Receiver<ProjectEvent>,
    task: slint::JoinHandle<()>,
    worker: thread::JoinHandle<()>,
}

pub(crate) fn run(initial_folder: Option<PathBuf>) -> Result<ExitCode, Box<dyn Error>> {
    let window = MainWindow::new()?;
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    let state = Rc::new(RefCell::new(WindowState::default()));
    let opening = initial_folder
        .map(|folder| {
            window.set_file_list_status("Loading folder…".into());
            let state = Rc::clone(&state);
            open_folder(&window, folder, move |window, result| {
                finish_initial_open(window, &mut state.borrow_mut(), result);
            })
        })
        .transpose()?;

    let result = window.run();
    if let Some(opening) = opening {
        opening.finish(&window)?;
    }
    result?;
    if state.borrow().initial_open_failed {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn open_folder(
    window: &MainWindow,
    folder: PathBuf,
    completed: impl FnOnce(&MainWindow, Result<Option<LoadedProject>, OpenError>) + 'static,
) -> Result<ProjectOpen, Box<dyn Error>> {
    let (events, requests) = bounded(1);
    let task = slint::spawn_local(handle_events(window.as_weak(), requests.clone(), completed))?;
    let worker = thread::Builder::new()
        .name("open project".into())
        .spawn(move || {
            let result = load_project(&folder, &events);
            let _ = events.send_blocking(ProjectEvent::Finished(result));
        })?;
    Ok(ProjectOpen {
        requests,
        task,
        worker,
    })
}

impl ProjectOpen {
    fn finish(self, window: &MainWindow) -> io::Result<()> {
        self.requests.close();
        window.on_message_dismissed(|_| {});
        // Dropping an undelivered confirmation also releases its waiting worker.
        drop(self.requests.try_recv());
        self.task.abort();
        self.worker
            .join()
            .map_err(|_| io::Error::other("project open worker failed"))
    }
}

async fn handle_events(
    window: Weak<MainWindow>,
    requests: Receiver<ProjectEvent>,
    completed: impl FnOnce(&MainWindow, Result<Option<LoadedProject>, OpenError>),
) {
    while let Ok(event) = requests.recv().await {
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
            ProjectEvent::Finished(result) => {
                completed(&window, result);
                break;
            }
        }
    }
}

fn finish_initial_open(
    window: &MainWindow,
    state: &mut WindowState,
    result: Result<Option<LoadedProject>, OpenError>,
) {
    window.set_file_list_status("".into());
    match result {
        Ok(Some(LoadedProject {
            project,
            photographs,
        })) => {
            let root = project.root();
            let name = root
                .file_name()
                .unwrap_or(root.as_os_str())
                .to_string_lossy();
            window.set_project_title(name.as_ref().into());
            let status = match photographs.total {
                0 => "No photos found".into(),
                total if total > i64::from(FILE_LIST_LIMIT) => {
                    format!("Showing {FILE_LIST_LIMIT} of {total} photos").into()
                }
                _ => "".into(),
            };
            let rows: Vec<_> = photographs
                .paths
                .into_iter()
                .map(|path| StandardListViewItem::from(path.as_str()))
                .collect();
            window.set_photo_files(ModelRc::new(VecModel::from(rows)));
            window.set_file_list_status(status);
            state.project = Some(project);
        }
        Ok(None) => {
            let _ = window.hide();
        }
        Err(error) => {
            state.initial_open_failed = true;
            eprintln!("CuttingRoom could not open the project. {error}");
            let weak = window.as_weak();
            window.on_message_dismissed(move |_| {
                if let Some(window) = weak.upgrade() {
                    let _ = window.hide();
                }
            });
            window.invoke_show_message(Message {
                severity: MessageSeverity::Error,
                heading: "Could not open project".into(),
                text: error.to_string().into(),
                ..Message::default()
            });
        }
    }
}

fn load_project(
    folder: &Path,
    events: &Sender<ProjectEvent>,
) -> Result<Option<LoadedProject>, OpenError> {
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
    let project = match outcome {
        OpenOutcome::Ready(project) => project,
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
                upgrade.apply()?
            } else {
                return Ok(None);
            }
        }
        OpenOutcome::Cancelled => return Ok(None),
    };
    match project.sync_photographs(FILE_LIST_LIMIT, || events.is_closed()) {
        Ok(photographs) => Ok(Some(LoadedProject {
            project,
            photographs,
        })),
        Err(OpenError::ScanCancelled) => Ok(None),
        Err(error) => Err(error),
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
