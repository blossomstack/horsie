use models::executor::ExecutorEvent;

pub trait ExecutorEventHandler: Send + Sync {
    fn on_event(&self, executor_id: &str, request_id: &str, event: &ExecutorEvent);
}
