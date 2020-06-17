// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod idata_handler;
mod idata_holder;
mod idata_op;
mod mdata_handler;
mod sdata_handler;

use crate::{action::Action, rpc::Rpc, utils, vault::Init, Config, Result};
use idata_handler::IDataHandler;
use idata_holder::IDataHolder;
use idata_op::{IDataOp, OpType};
use log::{debug, error, trace};
use mdata_handler::MDataHandler;
use routing::{Node, SrcLocation};
use sdata_handler::SDataHandler;
use tiny_keccak::sha3_256;

use safe_nd::{
    IDataAddress, IDataRequest, MessageId, NodePublicId, PublicId, Request, Response, XorName,
};

use std::{
    cell::{Cell, RefCell},
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    fmt::{self, Display, Formatter},
    rc::Rc,
};

pub(crate) struct DataHandler {
    id: NodePublicId,
    idata_holder: IDataHolder,
    idata_handler: Option<IDataHandler>,
    mdata_handler: Option<MDataHandler>,
    sdata_handler: Option<SDataHandler>,
    idata_copy_op: BTreeMap<MessageId, PublicId>,
}

impl DataHandler {
    pub fn new(
        id: NodePublicId,
        config: &Config,
        total_used_space: &Rc<Cell<u64>>,
        init_mode: Init,
        is_elder: bool,
        routing_node: Rc<RefCell<Node>>,
    ) -> Result<Self> {
        let idata_holder = IDataHolder::new(id.clone(), config, total_used_space, init_mode)?;
        let (idata_handler, mdata_handler, sdata_handler) = if is_elder {
            let idata_handler = IDataHandler::new(id.clone(), config, init_mode, routing_node)?;
            let mdata_handler = MDataHandler::new(id.clone(), config, total_used_space, init_mode)?;
            let sdata_handler = SDataHandler::new(id.clone(), config, total_used_space, init_mode)?;

            (
                Some(idata_handler),
                Some(mdata_handler),
                Some(sdata_handler),
            )
        } else {
            (None, None, None)
        };

        Ok(Self {
            id,
            idata_holder,
            idata_handler,
            mdata_handler,
            sdata_handler,
            idata_copy_op: Default::default(),
        })
    }

    pub fn handle_vault_rpc(&mut self, src: SrcLocation, rpc: Rpc) -> Option<Action> {
        match rpc {
            Rpc::Request {
                request,
                requester,
                message_id,
            } => self.handle_request(src, requester, request, message_id),
            Rpc::Response {
                response,
                message_id,
                ..
            } => self.handle_response(utils::get_source_name(src), response, message_id),
            Rpc::Duplicate {
                requester,
                address,
                holders,
                message_id,
            } => self.handle_duplicate_request(requester, address, holders, message_id),
        }
    }

    fn handle_duplicate_request(
        &mut self,
        requester: PublicId,
        address: IDataAddress,
        holders: BTreeSet<XorName>,
        message_id: MessageId,
    ) -> Option<Action> {
        debug!("got the duplication rpc");
        trace!(
            "{}: Received duplication request from src {:?}",
            self,
            requester,
        );
        match self.idata_copy_op.entry(message_id) {
            Entry::Vacant(vacant_entry) => {
                trace!(
                    "Sending GetIData request for address: ({:?}) to {:?}",
                    address,
                    holders,
                );
                let our_name = self.id.name();
                let our_id = self.id.clone();
                let _ = vacant_entry.insert(requester);
                Some(Action::SendToPeers {
                    sender: *our_name,
                    targets: holders,
                    rpc: Rpc::Request {
                        request: Request::IData(IDataRequest::Get(address)),
                        requester: PublicId::Node(our_id),
                        message_id,
                    },
                })
            }
            Entry::Occupied(_) => None,
        }
    }

    fn handle_request(
        &mut self,
        src: SrcLocation,
        requester: PublicId,
        request: Request,
        message_id: MessageId,
    ) -> Option<Action> {
        use Request::*;
        trace!(
            "{}: Received ({:?} {:?}) from src {:?} (client {:?})",
            self,
            request,
            message_id,
            src,
            requester
        );
        match request {
            IData(idata_req) => {
                match idata_req {
                    IDataRequest::Put(data) => {
                        if matches!(src, SrcLocation::Section(_)) {
                            // Since the requester is a section, this message was sent by the data handlers to us
                            // as a single data handler, implying that we're a data holder chosen to store the
                            // chunk.
                            self.idata_holder.store_idata(&data, requester, message_id)
                        } else {
                            self.handle_idata_request(|idata_handler| {
                                idata_handler.handle_put_idata_req(requester, data, message_id)
                            })
                        }
                    }
                    IDataRequest::Get(address) => {
                        if matches!(src, SrcLocation::Section(_)) {
                            // Since the requester is a node, this message was sent by the data handlers to us
                            // as a single data handler, implying that we're a data holder where the chunk is
                            // stored.
                            self.idata_holder.get_idata(address, requester, message_id)
                        } else {
                            self.handle_idata_request(|idata_handler| {
                                idata_handler.handle_get_idata_req(requester, address, message_id)
                            })
                        }
                    }
                    IDataRequest::DeleteUnpub(address) => {
                        if matches!(src, SrcLocation::Section(_)) {
                            // Since the requester is a node, this message was sent by the data handlers to us
                            // as a single data handler, implying that we're a data holder where the chunk is
                            // stored.
                            self.idata_holder
                                .delete_unpub_idata(address, requester, message_id)
                        } else {
                            // We're acting as data handler, received request from client handlers
                            self.handle_idata_request(|idata_handler| {
                                idata_handler
                                    .handle_delete_unpub_idata_req(requester, address, message_id)
                            })
                        }
                    }
                }
            }
            MData(mdata_req) => self.mdata_handler.as_mut().map_or_else(
                || {
                    trace!("Not applicable to Adults");
                    None
                },
                |mdata_handler| mdata_handler.handle_request(requester, mdata_req, message_id),
            ),
            SData(sdata_req) => self.sdata_handler.as_mut().map_or_else(
                || {
                    trace!("Not applicable to Adults");
                    None
                },
                |sdata_handler| sdata_handler.handle_request(requester, sdata_req, message_id),
            ),
            Coins(_) | LoginPacket(_) | Client(_) => {
                error!(
                    "{}: Should not receive {:?} as a data handler.",
                    self, request
                );
                None
            }
        }
    }

    fn handle_idata_request<F>(&mut self, operation: F) -> Option<Action>
    where
        F: FnOnce(&mut IDataHandler) -> Option<Action>,
    {
        self.idata_handler.as_mut().map_or_else(
            || {
                trace!("Not applicable to Adults");
                None
            },
            |idata_handler| operation(idata_handler),
        )
    }

    fn handle_response(
        &mut self,
        src: XorName,
        response: Response,
        message_id: MessageId,
    ) -> Option<Action> {
        use Response::*;
        trace!(
            "{}: Received ({:?} {:?}) from {}",
            self,
            response,
            message_id,
            src
        );
        match response {
            Mutation(result) => self.handle_idata_request(|idata_handler| {
                idata_handler.handle_mutation_resp(src, result, message_id)
            }),
            GetIData(result) => {
                if self.idata_copy_op.contains_key(&message_id) {
                    debug!("got the duplication copy");
                    if let Ok(data) = result {
                        trace!(
                            "Got GetIData copy response for address: ({:?})",
                            data.address(),
                        );
                        let requester = self.idata_copy_op.get(&message_id).unwrap().clone();
                        self.idata_holder.store_idata(&data, requester, message_id)
                    } else {
                        None
                    }
                } else {
                    self.handle_idata_request(|idata_handler| {
                        idata_handler.handle_get_idata_resp(src, result, message_id)
                    })
                }
            }
            //
            // ===== Invalid =====
            //
            ref _other => {
                error!(
                    "{}: Should not receive {:?} as a data handler.",
                    self, response
                );
                None
            }
        }
    }

    // This should be called whenever a node leaves the section. It fetches the list of data that was
    // previously held by the node and requests the other holders to store an additional copy.
    // The list of holders is also updated by removing the node that left.
    pub fn trigger_chunk_duplication(&mut self, node: XorName) -> Option<Vec<Action>> {
        self.idata_handler.as_mut().map_or_else(
            || {
                trace!("Not applicable to Adults");
                None
            },
            |idata_handler| idata_handler.trigger_data_copy_process(node),
        )
    }
}

impl Display for DataHandler {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "{}", self.id.name())
    }
}
