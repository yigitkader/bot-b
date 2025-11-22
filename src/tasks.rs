use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct TaskInfo {
    pub name: String,
    pub handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

pub struct TaskManager {
    tasks: Vec<TaskInfo>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
        }
    }

    pub fn spawn<F>(&mut self, name: impl Into<String>, future: F) -> Arc<Mutex<Option<JoinHandle<()>>>>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::spawn(future);
        let handle_arc = Arc::new(Mutex::new(Some(handle)));
        self.tasks.push(TaskInfo {
            name: name.into(),
            handle: handle_arc.clone(),
        });
        handle_arc
    }

    pub fn spawn_with_result<F, E>(&mut self, name: impl Into<String>, future: F) -> Arc<Mutex<Option<JoinHandle<()>>>>
    where
        F: std::future::Future<Output = Result<(), E>> + Send + 'static,
        E: std::fmt::Debug + Send + 'static,
    {
        let name_str = name.into();
        let name_for_log = name_str.clone();
        let handle = tokio::spawn(async move {
            if let Err(err) = future.await {
                log::error!("Task '{}' failed: {:?}", name_for_log, err);
            }
        });
        let handle_arc = Arc::new(Mutex::new(Some(handle)));
        self.tasks.push(TaskInfo {
            name: name_str,
            handle: handle_arc.clone(),
        });
        handle_arc
    }

    pub fn add_task(&mut self, name: impl Into<String>, handle: Arc<Mutex<Option<JoinHandle<()>>>>) {
        self.tasks.push(TaskInfo {
            name: name.into(),
            handle,
        });
    }

    pub fn tasks(&self) -> &[TaskInfo] {
        &self.tasks
    }

    pub fn into_tasks(self) -> Vec<TaskInfo> {
        self.tasks
    }
}

