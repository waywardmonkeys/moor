use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Error};
use dashmap::DashMap;
use fast_counter::ConcurrentCounter;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tracing::{debug, error, instrument, trace};

use crate::db::matching::{world_environment_match_object, MatchEnvironment};
use crate::db::state::{WorldState, WorldStateSource};
use crate::model::objects::ObjFlag;
use crate::model::var::{Objid, Var, NOTHING};
use crate::server::parse_cmd::{parse_command, ParsedCommand};
use crate::server::Sessions;
use crate::util::bitenum::BitEnum;
use crate::vm::execute::{ExecutionResult, FinallyReason, VM};

type TaskId = usize;

#[derive(Debug)]
enum TaskControlMsg {
    StartCommandVerb {
        player: Objid,
        vloc: Objid,
        command: ParsedCommand,
    },
    StartVerb {
        player: Objid,
        vloc: Objid,
        verb: String,
        args: Vec<Var>,
    },
    Abort,
}

#[derive(Debug)]
enum TaskControlResponse {
    Success(Var),
    Exception(FinallyReason),
    AbortError(Error),
    AbortCancelled,
}

pub struct Task {
    task_id: TaskId,
    control_receiver: UnboundedReceiver<TaskControlMsg>,
    response_sender: UnboundedSender<(TaskId, TaskControlResponse)>,
    player: Objid,
    vm: Arc<Mutex<VM>>,
    sessions: Arc<Mutex<dyn Sessions + Send + Sync>>,
}

struct TaskControl {
    pub task: Arc<Mutex<Task>>,
    pub control_sender: UnboundedSender<TaskControlMsg>,
}

pub struct Scheduler {
    running: AtomicBool,
    state_source: Arc<Mutex<dyn WorldStateSource + Send + Sync>>,
    next_task_id: AtomicUsize,
    tasks: DashMap<TaskId, TaskControl>,
    response_sender: UnboundedSender<(TaskId, TaskControlResponse)>,
    response_receiver: UnboundedReceiver<(TaskId, TaskControlResponse)>,

    num_scheduled_tasks: ConcurrentCounter,
    num_started_tasks: ConcurrentCounter,
    num_succeeded_tasks: ConcurrentCounter,
    num_aborted_tasks: ConcurrentCounter,
    num_errored_tasks: ConcurrentCounter,
    num_excepted_tasks: ConcurrentCounter,
}

struct DBMatchEnvironment<'a> {
    ws: &'a mut dyn WorldState,
}

impl<'a> MatchEnvironment for DBMatchEnvironment<'a> {
    fn obj_valid(&mut self, oid: Objid) -> Result<bool, Error> {
        self.ws.valid(oid).map_err(|e| anyhow!(e))
    }

    fn get_names(&mut self, oid: Objid) -> Result<Vec<String>, Error> {
        let mut names = self.ws.names_of(oid)?;
        let mut object_names = vec![names.0];
        object_names.append(&mut names.1);
        Ok(object_names)
    }

    fn get_surroundings(&mut self, player: Objid) -> Result<Vec<Objid>, Error> {
        let location = self.ws.location_of(player)?;
        let mut surroundings = self.ws.contents_of(location)?;
        surroundings.push(location);
        surroundings.push(player);

        Ok(surroundings)
    }

    fn location_of(&mut self, player: Objid) -> Result<Objid, Error> {
        Ok(self.ws.location_of(player)?)
    }
}

impl Scheduler {
    pub fn new(state_source: Arc<Mutex<dyn WorldStateSource + Sync + Send>>) -> Self {
        let (response_sender, response_receiver) = tokio::sync::mpsc::unbounded_channel();
        Self {
            running: Default::default(),
            state_source,
            next_task_id: Default::default(),
            tasks: DashMap::new(),
            response_sender,
            response_receiver,
            num_scheduled_tasks: ConcurrentCounter::new(0),
            num_started_tasks: ConcurrentCounter::new(0),
            num_succeeded_tasks: ConcurrentCounter::new(0),
            num_aborted_tasks: ConcurrentCounter::new(0),
            num_errored_tasks: ConcurrentCounter::new(0),
            num_excepted_tasks: ConcurrentCounter::new(0),
        }
    }

    #[instrument(skip(self, sessions))]
    pub async fn setup_command_task(
        &mut self,
        player: Objid,
        command: &str,
        sessions: Arc<Mutex<dyn Sessions + Send + Sync>>,
    ) -> Result<TaskId, anyhow::Error> {
        let (vloc, command) = {
            let mut ss = self.state_source.lock().await;
            let mut ws = ss.new_world_state().unwrap();
            let mut me = DBMatchEnvironment { ws: ws.as_mut() };
            let match_object_fn =
                |name: &str| world_environment_match_object(&mut me, player, name).unwrap();
            let pc = parse_command(command, match_object_fn);

            let loc = ws.location_of(player)?;
            let mut vloc = NOTHING;
            if let Some(_vh) = ws.find_command_verb_on(player, &pc)? {
                vloc = player;
            } else if let Some(_vh) = ws.find_command_verb_on(loc, &pc)? {
                vloc = loc;
            } else if let Some(_vh) = ws.find_command_verb_on(pc.dobj, &pc)? {
                vloc = pc.dobj;
            } else if let Some(_vh) = ws.find_command_verb_on(pc.iobj, &pc)? {
                vloc = pc.iobj;
            }

            if vloc == NOTHING {
                return Err(anyhow!("Could not parse command: {:?}", pc));
            }

            (vloc, pc)
        };
        let task_id = self
            .new_task(player, self.state_source.clone(), sessions)
            .await?;

        let Some(task_ref) = self.tasks.get_mut(&task_id) else {
            return Err(anyhow!("Could not find task with id {:?}", task_id));
        };

        // This gets enqueued as the first thing the task sees when it is started.
        task_ref
            .control_sender
            .send(TaskControlMsg::StartCommandVerb {
                player,
                vloc,
                command,
            })?;

        Ok(task_id)
    }

    #[instrument(skip(self, sessions))]
    pub async fn setup_verb_task(
        &mut self,
        player: Objid,
        vloc: Objid,
        verb: String,
        args: Vec<Var>,
        sessions: Arc<Mutex<dyn Sessions + Send + Sync>>,
    ) -> Result<TaskId, anyhow::Error> {
        let task_id = self
            .new_task(player, self.state_source.clone(), sessions)
            .await?;

        let Some(task_ref) = self.tasks.get_mut(&task_id) else {
            return Err(anyhow!("Could not find task with id {:?}", task_id));
        };

        // This gets enqueued as the first thing the task sees when it is started.
        task_ref.control_sender.send(TaskControlMsg::StartVerb {
            player,
            vloc,
            verb,
            args,
        })?;

        Ok(task_id)
    }

    #[instrument(skip(self))]
    pub(crate) async fn do_process(&mut self) -> Result<(), anyhow::Error> {
        let msg = match self.response_receiver.try_recv() {
            Ok(msg) => msg,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(e) => {
                return Err(anyhow!(e));
            }
        };
        match msg {
            (task_id, TaskControlResponse::AbortCancelled) => {
                self.num_aborted_tasks.add(1);

                debug!("Cleaning up cancelled task {:?}", task_id);
                self.remove_task(task_id)
                    .await
                    .expect("Could not remove task");
            }
            (task_id, TaskControlResponse::AbortError(e)) => {
                self.num_errored_tasks.add(1);

                error!("Error in task {:?}: {:?}", task_id, e);
                self.remove_task(task_id)
                    .await
                    .expect("Could not remove task");
            }
            (task_id, TaskControlResponse::Exception(finally_reason)) => {
                self.num_excepted_tasks.add(1);

                error!("Exception in task {:?}: {:?}", task_id, finally_reason);
                self.remove_task(task_id)
                    .await
                    .expect("Could not remove task");
            }
            (task_id, TaskControlResponse::Success(value)) => {
                self.num_succeeded_tasks.add(1);
                debug!(
                    "Task {:?} completed successfully with return value: {:?}",
                    task_id, value
                );
                self.remove_task(task_id)
                    .await
                    .expect("Could not remove task");
            }
        }
        Ok(())
    }

    pub async fn stop(scheduler: Arc<Mutex<Self>>) -> Result<(), anyhow::Error> {
        let scheduler = scheduler.lock().await;
        scheduler.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn new_task(
        &mut self,
        player: Objid,
        state_source: Arc<Mutex<dyn WorldStateSource + Send + Sync>>,
        client_connection: Arc<Mutex<dyn Sessions + Send + Sync>>,
    ) -> Result<TaskId, anyhow::Error> {
        let mut state_source = state_source.lock().await;
        let state = state_source.new_world_state()?;
        let vm = Arc::new(Mutex::new(VM::new(state)));

        let (tx_control, rx_control) = tokio::sync::mpsc::unbounded_channel();

        let task_id = self.next_task_id.fetch_add(1, Ordering::SeqCst);
        let task = Task {
            task_id,
            control_receiver: rx_control,
            response_sender: self.response_sender.clone(),
            player,
            vm,
            sessions: client_connection,
        };
        let task_info = TaskControl {
            task: Arc::new(Mutex::new(task)),
            control_sender: tx_control,
        };

        self.num_scheduled_tasks.add(1);

        self.tasks.insert(task_id, task_info);

        Ok(task_id)
    }

    #[instrument(skip(self), name="scheduler_start_task", fields(task_id = task_id))]
    pub async fn start_task(&mut self, task_id: TaskId) -> Result<(), anyhow::Error> {
        let task = {
            let Some(task_ref) = self.tasks.get_mut(&task_id) else {
                return Err(anyhow!("Could not find task with id {:?}", task_id));
            };
            task_ref.task.clone()
        };

        // Spawn the task's thread.
        tokio::spawn(async move {
            debug!("Starting up task: {:?}", task_id);
            task.lock().await.run(task_id).await;

            debug!("Completed task: {:?}", task_id);
        })
        .await?;

        self.num_started_tasks.add(1);
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn abort_task(&mut self, id: TaskId) -> Result<(), anyhow::Error> {
        let task = self
            .tasks
            .get_mut(&id)
            .ok_or(anyhow::anyhow!("Task not found"))?;
        task.control_sender.send(TaskControlMsg::Abort)?;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn remove_task(&mut self, id: TaskId) -> Result<(), anyhow::Error> {
        self.tasks
            .remove(&id)
            .ok_or(anyhow::anyhow!("Task not found"))?;
        Ok(())
    }
}

impl Task {
    #[instrument(skip(self), name="task_run", fields(task_id = task_id))]
    pub async fn run(&mut self, task_id: TaskId) {
        trace!("Entering task loop...");
        let mut vm = self.vm.lock().await;
        let mut running_method = false;
        loop {
            let msg = if running_method {
                match self.control_receiver.try_recv() {
                    Ok(msg) => Some(msg),
                    Err(TryRecvError::Empty) => None,
                    Err(_) => panic!("Task control channel closed"),
                }
            } else {
                self.control_receiver.recv().await
            };
            // Check for control messages.
            match msg {
                // We've been asked to start a command.
                // We need to set up the VM and then execute it.
                Some(TaskControlMsg::StartCommandVerb {
                    player,
                    vloc,
                    command,
                }) => {
                    // We should never be asked to start a command while we're already running one.
                    assert!(!running_method);
                    vm.do_method_verb(
                        vloc,
                        command.verb.as_str(),
                        false,
                        vloc,
                        player,
                        BitEnum::new_with(ObjFlag::Wizard),
                        player,
                        command.args,
                    )
                    .expect("Could not set up VM for command execution");
                    running_method = true;
                }

                Some(TaskControlMsg::StartVerb {
                    player,
                    vloc,
                    verb,
                    args,
                }) => {
                    // We should never be asked to start a command while we're already running one.
                    assert!(!running_method);
                    vm.do_method_verb(
                        vloc,
                        verb.as_str(),
                        false,
                        vloc,
                        player,
                        BitEnum::new_with(ObjFlag::Wizard),
                        player,
                        args,
                    )
                    .expect("Could not set up VM for command execution");
                    running_method = true;
                }
                // We've been asked to die.
                Some(TaskControlMsg::Abort) => {
                    vm.rollback().unwrap();

                    self.response_sender
                        .send((self.task_id, TaskControlResponse::AbortCancelled))
                        .expect("Could not send abort response");
                    return;
                }
                _ => {}
            }

            if !running_method {
                continue;
            }
            let result = vm.exec(self.sessions.clone()).await;
            match result {
                Ok(ExecutionResult::More) => {}
                Ok(ExecutionResult::Complete(a)) => {
                    vm.commit().unwrap();

                    debug!("Task {} complete with result: {:?}", task_id, a);

                    self.response_sender
                        .send((self.task_id, TaskControlResponse::Success(a)))
                        .expect("Could not send success response");
                    return;
                }
                Ok(ExecutionResult::Exception(e)) => {
                    vm.rollback().unwrap();

                    debug!("Task finished with exception {:?}", e);
                    self.sessions
                        .lock()
                        .await
                        .send_text(self.player, format!("Exception: {:?}", e).to_string())
                        .await
                        .unwrap();

                    self.response_sender
                        .send((self.task_id, TaskControlResponse::Exception(e)))
                        .expect("Could not send exception response");

                    return;
                }
                Err(e) => {
                    vm.rollback().unwrap();
                    error!("Task {} failed with error: {:?}", task_id, e);

                    self.response_sender
                        .send((self.task_id, TaskControlResponse::AbortError(e)))
                        .expect("Could not send error response");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Error;
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use crate::compiler::codegen::compile;
    use crate::db::inmem_db::ImDB;
    use crate::db::inmem_db_worldstate::ImDbWorldStateSource;
    use crate::model::objects::{ObjAttrs, ObjFlag};
    use crate::model::r#match::{ArgSpec, PrepSpec, VerbArgsSpec};
    use crate::model::var::{Objid, NOTHING};
    use crate::model::verbs::VerbFlag;
    use crate::server::scheduler::Scheduler;
    use crate::server::Sessions;
    use crate::util::bitenum::BitEnum;

    struct NoopClientConnection {}
    impl NoopClientConnection {
        pub fn new() -> Self {
            Self {}
        }
    }

    #[async_trait]
    impl Sessions for NoopClientConnection {
        async fn send_text(&mut self, _player: Objid, _msg: String) -> Result<(), anyhow::Error> {
            Ok(())
        }

        async fn connected_players(&mut self) -> Result<Vec<Objid>, Error> {
            Ok(vec![])
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_scheduler_loop() {
        let mut db = ImDB::new();

        let mut tx = db.do_begin_tx().unwrap();
        let sys_obj = db
            .create_object(
                &mut tx,
                None,
                ObjAttrs::new()
                    .location(NOTHING)
                    .parent(NOTHING)
                    .name("System")
                    .flags(BitEnum::new_with(ObjFlag::Read)),
            )
            .unwrap();
        db.add_verb(
            &mut tx,
            sys_obj,
            vec!["test"],
            sys_obj,
            BitEnum::new_with(VerbFlag::Read),
            VerbArgsSpec {
                dobj: ArgSpec::This,
                prep: PrepSpec::None,
                iobj: ArgSpec::This,
            },
            compile("return {1,2,3,4};").unwrap(),
        )
        .unwrap();

        db.do_commit_tx(&mut tx).expect("Commit of test data");

        let src = ImDbWorldStateSource::new(db);

        let mut sched = Scheduler::new(Arc::new(Mutex::new(src)));
        let task = sched
            .setup_verb_task(
                sys_obj,
                sys_obj,
                "test".to_string(),
                vec![],
                Arc::new(Mutex::new(NoopClientConnection::new())),
            )
            .await
            .expect("setup command task");
        assert_eq!(sched.tasks.len(), 1);

        sched.start_task(task).await.unwrap();

        assert_eq!(sched.tasks.len(), 1);

        while !sched.tasks.is_empty() {
            sched.do_process().await.unwrap();
        }

        assert_eq!(sched.tasks.len(), 0);
        assert_eq!(sched.num_started_tasks.sum(), 1);
        assert_eq!(sched.num_succeeded_tasks.sum(), 1);
        assert_eq!(sched.num_errored_tasks.sum(), 0);
        assert_eq!(sched.num_excepted_tasks.sum(), 0);
        assert_eq!(sched.num_aborted_tasks.sum(), 0);
    }
}
