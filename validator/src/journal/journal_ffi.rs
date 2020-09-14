/*
 * Copyright 2020 Cargill Incorporated
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 * ------------------------------------------------------------------------------
 */
use std::ffi::CStr;
use std::fs::File;
use std::io::Read;
use std::mem;
use std::os::raw::{c_char, c_void};
use std::ptr;
use std::slice;
use std::time::Duration;

use cpython::{self, ObjectProtocol, PyList, PyObject, Python, PythonObject, ToPyObject};
use py_ffi;
use pylogger;
use sawtooth::journal::commit_store::CommitStore;
use transact::{
    context::manager::sync::ContextManager,
    database::lmdb::LmdbDatabase,
    execution::adapter::static_adapter::StaticExecutionAdapter,
    execution::executor::Executor,
    protocol::batch::Batch,
    sawtooth::SawtoothToTransactHandlerAdapter,
    scheduler::serial::SerialSchedulerFactory,
    state::merkle::{MerkleRadixTree, MerkleState},
};

use battleship::handler::BattleshipTransactionHandler;
use block_info_tp::handler::BlockInfoTransactionHandler;
use cylinder::{secp256k1::Secp256k1Context, Context, PrivateKey, Signer};
use protobuf::Message;
use sawtooth::{
    consensus::notifier::BackgroundConsensusNotifier,
    journal::{
        block_manager::BlockManager,
        block_validator::{BlockValidationResultStore, BlockValidator},
        block_wrapper::BlockStatus,
        chain::*,
        chain_id_manager::ChainIdManager,
        genesis::{builder::GenesisControllerBuilder, GenesisController},
        publisher::{
            batch_injector::DefaultBatchInjectorFactory, BatchObserver, BlockBroadcaster,
            BlockPublisher, BlockPublisherError,
        },
    },
    protocol::block::BlockPair,
    protos::{FromBytes, IntoBytes},
    state::merkle::CborMerkleState,
    state::state_pruning_manager::StatePruningManager,
    state::state_view_factory::StateViewFactory,
};
use sawtooth_identity::handler::IdentityTransactionHandler;
use sawtooth_intkey::handler::IntkeyTransactionHandler;
use sawtooth_sabre::handler::SabreTransactionHandler;
use sawtooth_settings::handler::SettingsTransactionHandler;
use sawtooth_smallbank::handler::SmallbankTransactionHandler;
use sawtooth_xo::handler::XoTransactionHandler;

use proto::events::{Event, Event_Attribute};
use proto::transaction_receipt::{StateChange, StateChange_Type, TransactionReceipt};

use py_object_wrapper::PyObjectWrapper;

struct Journal {
    pub chain_controller: ChainController,
    pub block_publisher: BlockPublisher,
    pub genesis_controller: GenesisController,
}

impl Journal {
    fn start(&mut self) -> ErrorCode {
        match self.genesis_controller.requires_genesis() {
            Ok(create_genesis) => {
                if create_genesis {
                    if let Err(err) = self.genesis_controller.start() {
                        error!("Unable to create genesis block: {}", err);
                        return ErrorCode::Unknown;
                    }
                }
            }
            Err(err) => {
                error!(
                    "Unable to check if the genesis block should be built: {}",
                    err
                );
                return ErrorCode::Unknown;
            }
        }
        self.chain_controller.start();
        ErrorCode::Success
    }

    fn stop(&mut self) {
        self.chain_controller.stop();
        self.block_publisher.shutdown_signaler().shutdown();
    }
}

#[repr(u32)]
#[derive(Debug)]
pub enum ErrorCode {
    Success = 0,
    NullPointerProvided = 0x01,
    InvalidDataDir = 0x02,
    InvalidPythonObject = 0x03,
    InvalidBlockId = 0x04,
    #[allow(dead_code)]
    UnknownBlock = 0x05,

    Unknown = 0xff,
}

macro_rules! check_null {
     ($($arg:expr) , *) => {
         $(if $arg.is_null() { return ErrorCode::NullPointerProvided; })*
     }
 }

#[no_mangle]
pub unsafe extern "C" fn journal_new(
    commit_store: *mut c_void,
    block_manager: *const c_void,
    state_database: *const c_void,
    block_validation_result_cache: *const c_void,
    consensus_notifier_service: *mut c_void,
    block_sender: *mut py_ffi::PyObject,
    batch_observers: *mut py_ffi::PyObject,
    observers: *mut py_ffi::PyObject,
    state_pruning_block_depth: u32,
    fork_cache_keep_time: u32,
    data_directory: *const c_char,
    key_directory: *const c_char,
    genesis_observers: *mut py_ffi::PyObject,
    journal_ptr: *mut *const c_void,
) -> ErrorCode {
    check_null!(
        commit_store,
        block_manager,
        state_database,
        consensus_notifier_service,
        block_sender,
        batch_observers,
        observers,
        data_directory,
        key_directory,
        genesis_observers
    );

    let data_dir = match CStr::from_ptr(data_directory).to_str() {
        Ok(s) => s,
        Err(_) => return ErrorCode::InvalidDataDir,
    };

    let key_dir = match CStr::from_ptr(key_directory).to_str() {
        Ok(s) => s,
        Err(_) => return ErrorCode::InvalidDataDir,
    };

    let py = Python::assume_gil_acquired();

    let py_observers = PyObject::from_borrowed_ptr(py, observers);

    let consensus_notifier_service =
        Box::from_raw(consensus_notifier_service as *mut BackgroundConsensusNotifier);
    let block_status_store =
        (*(block_validation_result_cache as *const BlockValidationResultStore)).clone();

    let block_broadcaster = PyBlockBroadcaster {
        py_block_sender: PyObject::from_borrowed_ptr(py, block_sender),
    };

    let py_batch_observers = PyObject::from_borrowed_ptr(py, batch_observers);
    let batch_observers = if let Ok(py_list) = py_batch_observers.extract::<PyList>(py) {
        let mut res: Vec<Box<dyn BatchObserver>> = Vec::with_capacity(py_list.len(py));
        py_list
            .iter(py)
            .for_each(|pyobj| res.push(Box::new(PyBatchObserver::new(pyobj))));
        res
    } else {
        return ErrorCode::InvalidPythonObject;
    };

    let observer_wrappers = if let Ok(py_list) = py_observers.extract::<PyList>(py) {
        let mut res: Vec<Box<dyn ChainObserver>> = Vec::with_capacity(py_list.len(py));
        py_list
            .iter(py)
            .for_each(|pyobj| res.push(Box::new(PyChainObserver::new(pyobj))));
        res
    } else {
        return ErrorCode::InvalidPythonObject;
    };

    let block_manager = (*(block_manager as *const BlockManager)).clone();
    let state_database = (*(state_database as *const LmdbDatabase)).clone();
    let genesis_observer_wrappers = if let Ok(py_list) = py_observers.extract::<PyList>(py) {
        let mut res: Vec<Box<dyn ChainObserver>> = Vec::with_capacity(py_list.len(py));
        py_list
            .iter(py)
            .for_each(|pyobj| res.push(Box::new(PyChainObserver::new(pyobj))));
        res
    } else {
        return ErrorCode::InvalidPythonObject;
    };

    let state_view_factory = StateViewFactory::new(state_database.clone());
    let state_pruning_manager = StatePruningManager::new(state_database.clone());

    let commit_store = Box::from_raw(commit_store as *mut CommitStore);
    let merkle_state = CborMerkleState::new(MerkleState::new(Box::new(state_database.clone())));
    let context_manager = ContextManager::new(Box::new(merkle_state.clone()));

    let mut executor = match get_executor(context_manager.clone()) {
        Ok(executor) => executor,
        Err(error_code) => return error_code,
    };

    if let Err(err) = executor.start() {
        error!("Executor cannot start: {}", err);
        return ErrorCode::Unknown;
    };

    let block_validator_submitter = match executor.execution_task_submitter() {
        Ok(submitter) => submitter,
        Err(err) => {
            error!("Unable to get execution task submitter: {}", err);
            return ErrorCode::Unknown;
        }
    };

    let initial_state_root = match MerkleRadixTree::new(Box::new(state_database), None) {
        Ok(merkle_radix_tree) => merkle_radix_tree.get_merkle_root(),
        Err(err) => {
            error!("Unable to get initial state root hash: {}", err);
            return ErrorCode::Unknown;
        }
    };

    let identity_signer = match get_signer(key_dir) {
        Ok(signer) => signer,
        Err(error_code) => return error_code,
    };

    let block_validator = BlockValidator::new(
        block_manager.clone(),
        task_submitter.clone(),
        block_status_store.clone(),
        state_view_factory.clone(),
        Box::new(SerialSchedulerFactory::new(Box::new(
            context_manager.clone(),
        ))),
        initial_state_root.clone(),
        merkle_state.clone(),
    );

    let batch_injector_factory = Box::new(DefaultBatchInjectorFactory::new(
        identity_signer.clone(),
        state_view_factory,
    ));

    let block_publisher = BlockPublisher::builder()
        .with_block_manager(block_manager)
        .with_commit_store((*commit_store).clone())
        .with_block_broadcaster(Box::new(block_broadcaster))
        .with_batch_observers(batch_observers)
        .with_batch_injector_factory(batch_injector_factory)
        .with_merkle_state(merkle_state.clone())
        .with_execution_task_submitter(task_submitter)
        .with_scheduler_factory(Box::new(SerialSchedulerFactory::new(Box::new(
            context_manager.clone(),
        ))))
        .start()
        .expect("Unable to start block publisher");

    let chain_head_lock = block_publisher.get_chain_head_lock();
    let chain_controller = ChainController::new(
        block_manager.clone(),
        block_validator,
        commit_store.clone(),
        chain_head_lock,
        block_status_store,
        consensus_notifier_service.clone(),
        data_dir.into(),
        state_pruning_block_depth,
        observer_wrappers,
        state_pruning_manager,
        Duration::from_secs(u64::from(fork_cache_keep_time)),
        merkle_state.clone(),
        initial_state_root.to_string(),
    );

    let genesis_executor = match executor.execution_task_submitter() {
        Ok(submitter) => submitter,
        Err(err) => {
            error!(
                "Unable to get execution task submitter for genesis: {}",
                err
            );
            return ErrorCode::Unknown;
        }
    };
    let chain_id_manager = ChainIdManager::new(data_dir.into());

    let genesis_controller = match GenesisControllerBuilder::new()
        .with_transaction_executor(genesis_executor)
        .with_scheduler_factory(Box::new(SerialSchedulerFactory::new(Box::new(
            context_manager,
        ))))
        .with_block_manager(block_manager)
        .with_chain_reader(commit_store.clone())
        .with_state_view_factory(state_view_factory)
        .with_data_dir(data_dir.into())
        .with_observers(genesis_observer_wrappers)
        .with_initial_state_root(initial_state_root)
        .with_merkle_state(merkle_state)
        .with_identity_signer(identity_signer)
        .with_chain_id_manager(chain_id_manager)
        .build()
    {
        Ok(genesis_controller) => genesis_controller,
        Err(err) => {
            error!("Unable to build genesis controller: {}", err);
            return ErrorCode::Unknown;
        }
    };

    let journal = Journal {
        chain_controller,
        genesis_controller,
        block_publisher,
    };

    *journal_ptr = Box::into_raw(Box::new(journal)) as *const c_void;

    Box::into_raw(consensus_notifier_service);
    Box::into_raw(commit_store);

    ErrorCode::Success
}

fn get_executor(context_manager: ContextManager) -> Result<Executor, ErrorCode> {
    let execution_adapter = match StaticExecutionAdapter::new_adapter(
        vec![
            Box::new(SawtoothToTransactHandlerAdapter::new(
                SettingsTransactionHandler::new(),
            )),
            Box::new(SawtoothToTransactHandlerAdapter::new(
                SabreTransactionHandler::new(),
            )),
            Box::new(SawtoothToTransactHandlerAdapter::new(
                BlockInfoTransactionHandler::new(),
            )),
            Box::new(SawtoothToTransactHandlerAdapter::new(
                BattleshipTransactionHandler::new(),
            )),
            Box::new(SawtoothToTransactHandlerAdapter::new(
                IdentityTransactionHandler::new(),
            )),
            Box::new(SawtoothToTransactHandlerAdapter::new(
                SmallbankTransactionHandler::new(),
            )),
            Box::new(SawtoothToTransactHandlerAdapter::new(
                IntkeyTransactionHandler::new(),
            )),
            Box::new(SawtoothToTransactHandlerAdapter::new(
                XoTransactionHandler::new(),
            )),
        ],
        context_manager,
    ) {
        Ok(executor_adapter) => executor_adapter,
        Err(err) => {
            error!("Unable to create executor adapter: {}", err);
            return Err(ErrorCode::Unknown);
        }
    };

    Ok(Executor::new(vec![Box::new(execution_adapter)]))
}

fn get_signer(private_key_filename: &str) -> Result<Box<dyn Signer>, ErrorCode> {
    let private_key_filename = format!("{}/validator.priv", private_key_filename);
    let mut f = match File::open(&private_key_filename) {
        Ok(f) => f,
        Err(err) => {
            error!("Unable to open key file {}: {}", private_key_filename, err);
            return Err(ErrorCode::Unknown);
        }
    };

    let mut contents = String::new();
    match f.read_to_string(&mut contents) {
        Ok(_) => (),
        Err(err) => {
            error!("Unable to read key file {}: {}", private_key_filename, err);
            return Err(ErrorCode::Unknown);
        }
    };

    let key_str = match contents.lines().next() {
        Some(k) => k,
        None => {
            error!("Empty key file: {}", private_key_filename);
            return Err(ErrorCode::Unknown);
        }
    };

    let context = Secp256k1Context::new();
    let private_key = match PrivateKey::new_from_hex(key_str) {
        Ok(k) => k,
        Err(err) => {
            error!(
                "Unable to create private key from hex in {}: {}",
                private_key_filename, err
            );
            return Err(ErrorCode::Unknown);
        }
    };

    let key = context.new_signer(private_key);
    Ok(key)
}

#[no_mangle]
pub unsafe extern "C" fn journal_drop(journal: *mut c_void) -> ErrorCode {
    check_null!(journal);

    Box::from_raw(journal as *mut Journal);
    ErrorCode::Success
}

#[no_mangle]
pub unsafe extern "C" fn journal_start(journal: *mut c_void) -> ErrorCode {
    check_null!(journal);

    (*(journal as *mut Journal)).start()
}

#[no_mangle]
pub unsafe extern "C" fn chain_controller_block_validation_result(
    journal: *mut c_void,
    block_id: *const c_char,
    result: *mut i32,
) -> ErrorCode {
    let block_id = match CStr::from_ptr(block_id).to_str() {
        Ok(s) => s,
        Err(_) => return ErrorCode::InvalidBlockId,
    };

    let status = match (*(journal as *mut Journal))
        .chain_controller
        .block_validation_result(block_id)
    {
        Some(r) => r.status,
        None => BlockStatus::Unknown,
    };
    *result = status as i32;
    ErrorCode::Success
}

#[no_mangle]
pub unsafe extern "C" fn journal_stop(journal: *mut c_void) -> ErrorCode {
    check_null!(journal);

    (*(journal as *mut Journal)).stop();

    ErrorCode::Success
}

macro_rules! chain_controller_block_ffi {
     ($ffi_fn_name:ident, $cc_fn_name:ident, $block:ident, $($block_args:tt)*) => {
         #[no_mangle]
         pub unsafe extern "C" fn $ffi_fn_name(
             journal: *mut c_void,
             block_bytes: *const u8,
             block_bytes_len: usize,
         ) -> ErrorCode {
             check_null!(journal, block_bytes);

             let data = slice::from_raw_parts(block_bytes, block_bytes_len);
             let $block = match BlockPair::from_bytes(&data) {
                 Ok(block_pair) => block_pair,
                 Err(err) => {
                     error!("Failed to parse block bytes: {:?}", err);
                     return ErrorCode::Unknown;
                 }
             };

             (*(journal as *mut Journal)).chain_controller.$cc_fn_name($($block_args)*);

             ErrorCode::Success
         }
     }
 }

chain_controller_block_ffi!(
    chain_controller_validate_block,
    validate_block,
    block,
    &block
);
chain_controller_block_ffi!(chain_controller_ignore_block, ignore_block, block, &block);
chain_controller_block_ffi!(chain_controller_fail_block, fail_block, block, &block);
chain_controller_block_ffi!(chain_controller_commit_block, commit_block, block, block);

#[no_mangle]
pub unsafe extern "C" fn chain_controller_queue_block(
    journal: *mut c_void,
    block_id: *const c_char,
) -> ErrorCode {
    check_null!(journal, block_id);

    let block_id = match CStr::from_ptr(block_id).to_str() {
        Ok(s) => s,
        Err(_) => return ErrorCode::InvalidBlockId,
    };

    (*(journal as *mut Journal))
        .chain_controller
        .queue_block(block_id);

    ErrorCode::Success
}

/// This is only exposed for the current python tests, it should be removed
/// when proper rust tests are written for the ChainController
#[no_mangle]
pub unsafe extern "C" fn chain_controller_on_block_received(
    journal: *mut c_void,
    block_id: *const c_char,
) -> ErrorCode {
    check_null!(journal, block_id);

    let block_id = match CStr::from_ptr(block_id).to_str() {
        Ok(s) => s,
        Err(_) => return ErrorCode::InvalidBlockId,
    };

    (*(journal as *mut Journal))
        .chain_controller
        .queue_block(block_id);

    ErrorCode::Success
}

#[no_mangle]
pub unsafe extern "C" fn chain_controller_chain_head(
    journal: *mut c_void,
    block: *mut *const u8,
    block_len: *mut usize,
    block_cap: *mut usize,
) -> ErrorCode {
    check_null!(journal);
    if let Some(chain_head) = (*(journal as *mut Journal)).chain_controller.chain_head() {
        match chain_head.into_bytes() {
            Ok(payload) => {
                *block_cap = payload.capacity();
                *block_len = payload.len();
                *block = payload.as_slice().as_ptr();

                mem::forget(payload);

                ErrorCode::Success
            }
            Err(err) => {
                warn!("Failed to serialize block to bytes: {}", err);
                ErrorCode::Unknown
            }
        }
    } else {
        *block = ptr::null();
        *block_len = 0;
        ErrorCode::Success
    }
}

struct PyChainObserver {
    py_observer: PyObject,
}

impl PyChainObserver {
    fn new(py_observer: PyObject) -> Self {
        PyChainObserver { py_observer }
    }
}

impl ChainObserver for PyChainObserver {
    fn chain_update(
        &mut self,
        block: &BlockPair,
        receipts: &[sawtooth::protos::transaction_receipt::TransactionReceipt],
    ) {
        let gil_guard = Python::acquire_gil();
        let py = gil_guard.python();

        let wrapped_block = PyObjectWrapper::from(block.clone());
        let local_receipts: Vec<TransactionReceipt> = receipts
            .iter()
            .map(|receipt| TransactionReceipt::from(receipt.clone()))
            .collect();

        self.py_observer
            .call_method(py, "chain_update", (wrapped_block, &local_receipts), None)
            .map(|_| ())
            .map_err(|py_err| {
                pylogger::exception(py, "Unable to call observer.chain_update", py_err);
            })
            .unwrap_or(())
    }
}

impl ToPyObject for TransactionReceipt {
    type ObjectType = PyObject;

    fn to_py_object(&self, py: Python) -> PyObject {
        let txn_receipt_protobuf_mod = py
            .import("sawtooth_validator.protobuf.transaction_receipt_pb2")
            .expect("Unable to import transaction_receipt_pb2");
        let py_txn_receipt_class = txn_receipt_protobuf_mod
            .get(py, "TransactionReceipt")
            .expect("Unable to get TransactionReceipt");

        let py_txn_receipt = py_txn_receipt_class
            .call(py, cpython::NoArgs, None)
            .expect("Unable to instantiate TransactionReceipt");
        py_txn_receipt
            .call_method(
                py,
                "ParseFromString",
                (cpython::PyBytes::new(py, &self.write_to_bytes().unwrap()).into_object(),),
                None,
            )
            .expect("Unable to ParseFromString");

        py_txn_receipt
    }
}

impl From<sawtooth::protos::transaction_receipt::TransactionReceipt> for TransactionReceipt {
    fn from(
        txn_receipt: sawtooth::protos::transaction_receipt::TransactionReceipt,
    ) -> TransactionReceipt {
        let mut local_txn_receipt = TransactionReceipt::new();
        local_txn_receipt.set_state_changes(
            txn_receipt
                .state_changes
                .iter()
                .map(|sc| {
                    let mut state_change = StateChange::new();
                    state_change.set_address(sc.get_address().into());
                    state_change.set_value(sc.get_value().into());

                    match sc.field_type {
                        sawtooth::protos::transaction_receipt::StateChange_Type::TYPE_UNSET => {
                            state_change.set_field_type(StateChange_Type::TYPE_UNSET)
                        }
                        sawtooth::protos::transaction_receipt::StateChange_Type::SET => {
                            state_change.set_field_type(StateChange_Type::SET)
                        }
                        sawtooth::protos::transaction_receipt::StateChange_Type::DELETE => {
                            state_change.set_field_type(StateChange_Type::DELETE)
                        }
                    }
                    state_change
                })
                .collect(),
        );
        local_txn_receipt.set_events(
            txn_receipt
                .events
                .iter()
                .map(|e| {
                    let mut event = Event::new();
                    event.set_event_type(e.get_event_type().into());
                    event.set_data(e.get_data().into());
                    event.set_attributes(
                        e.get_attributes()
                            .iter()
                            .map(|at| {
                                let mut attributes = Event_Attribute::new();
                                attributes.set_key(at.get_key().into());
                                attributes.set_value(at.get_value().into());
                                attributes
                            })
                            .collect(),
                    );
                    event
                })
                .collect(),
        );
        local_txn_receipt.set_data(txn_receipt.data);
        local_txn_receipt.set_transaction_id(txn_receipt.transaction_id);

        local_txn_receipt
    }
}

#[no_mangle]
pub unsafe extern "C" fn block_status_store_new(
    block_status_store_ptr: *mut *const c_void,
) -> ErrorCode {
    let block_status_store = BlockValidationResultStore::new();

    *block_status_store_ptr = Box::into_raw(Box::new(block_status_store)) as *const c_void;
    ErrorCode::Success
}

#[no_mangle]
pub unsafe extern "C" fn block_status_store_drop(block_status_store_ptr: *mut c_void) -> ErrorCode {
    check_null!(block_status_store_ptr);

    Box::from_raw(block_status_store_ptr as *mut BlockValidationResultStore);
    ErrorCode::Success
}

/// wraps the python block sender to provide the BlockBroadcaster trait to the publisher.
struct PyBlockBroadcaster {
    py_block_sender: PyObject,
}

impl BlockBroadcaster for PyBlockBroadcaster {
    fn broadcast(&self, block: BlockPair) -> Result<(), BlockPublisherError> {
        let py_block: PyObjectWrapper = block.into();

        let gil = Python::acquire_gil();
        let py = gil.python();

        self.py_block_sender
            .call_method(py, "send", (&py_block,), None)
            .map(|_| ())
            .map_err(|py_err| {
                ::pylogger::exception(py, "{:?}", py_err);
                BlockPublisherError::Internal(
                    "Unable to broadcast block due to python error".into(),
                )
            })
    }
}

struct PyBatchObserver {
    py_batch_observer: PyObject,
}

impl PyBatchObserver {
    fn new(py_batch_observer: PyObject) -> Self {
        PyBatchObserver { py_batch_observer }
    }
}

impl BatchObserver for PyBatchObserver {
    fn notify_batch_pending(&self, batch: &Batch) {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let batch_wrapper = PyObjectWrapper::from(batch.clone());
        self.py_batch_observer
            .call_method(py, "notify_batch_pending", (batch_wrapper,), None)
            .expect("BatchObserver has no method notify_batch_pending");
    }
}
