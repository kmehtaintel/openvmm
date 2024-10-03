// Copyright (C) Microsoft Corporation. All rights reserved.

#![forbid(unsafe_code)]

mod channel_bitmap;
mod channels;
pub mod hvsock;
mod monitor;
mod proxyintegration;

/// The GUID type used for vmbus channel identifiers.
pub type Guid = guid::Guid;

use anyhow::Context;
use async_trait::async_trait;
use channel_bitmap::ChannelBitmap;
use channels::ConnectionTarget;
pub use channels::InitiateContactRequest;
use channels::InterruptPageError;
use channels::ModifyConnectionRequest;
pub use channels::ModifyConnectionResponse;
use channels::OfferId;
pub use channels::OfferParamsInternal;
use channels::OpenParams;
use channels::RestoreError;
pub use channels::Update;
use futures::channel::mpsc;
use futures::future::OptionFuture;
use futures::stream::SelectAll;
use futures::FutureExt;
use futures::StreamExt;
use guestmem::GuestMemory;
use hvdef::Vtl;
use inspect::Inspect;
use mesh::error::RemoteError;
use mesh::error::RemoteResult;
use mesh::error::RemoteResultExt;
use mesh::payload::Protobuf;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use mesh::RecvError;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_event::Event;
use ring::PAGE_SIZE;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use unicycle::FuturesUnordered;
use vmbus_channel::bus::ChannelRequest;
use vmbus_channel::bus::ChannelServerRequest;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::ModifyRequest;
use vmbus_channel::bus::OfferInput;
use vmbus_channel::bus::OfferKey;
use vmbus_channel::bus::OfferResources;
use vmbus_channel::bus::OpenData;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::bus::ParentBus;
use vmbus_channel::bus::RestoreResult;
use vmbus_channel::gpadl::GpadlMap;
use vmbus_channel::gpadl_ring::AlignedGpadlView;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_core::protocol;
pub use vmbus_core::protocol::GpadlId;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbus_core::MaxVersionInfo;
use vmbus_core::MonitorPageGpas;
use vmbus_core::OutgoingMessage;
use vmbus_core::TaggedStream;
#[cfg(windows)]
use vmbus_proxy::ProxyHandle;
use vmbus_ring as ring;
use vmbus_ring::gparange::MultiPagedRangeBuf;
use vmcore::interrupt::Interrupt;
use vmcore::save_restore::SavedStateRoot;
use vmcore::synic::EventPort;
use vmcore::synic::GuestEventPort;
use vmcore::synic::MessagePort;
use vmcore::synic::SynicPortAccess;

const SINT: u8 = 2;
pub const REDIRECT_SINT: u8 = 7;
pub const REDIRECT_VTL: Vtl = Vtl::Vtl2;
const SHARED_EVENT_CONNECTION_ID: u32 = 2;

const MAX_CONCURRENT_HVSOCK_REQUESTS: usize = 16;

pub struct VmbusServer {
    task_send: mesh::Sender<VmbusRequest>,
    control: Arc<VmbusServerControl>,
    _message_port: Box<dyn Sync + Send>,
    _multiclient_message_port: Option<Box<dyn Sync + Send>>,
    task: Task<ServerTask>,
}

pub struct VmbusServerBuilder<'a, T: Spawn> {
    spawner: &'a T,
    synic: Arc<dyn SynicPortAccess>,
    gm: GuestMemory,
    trusted_gm: Option<GuestMemory>,
    vtl: Vtl,
    hvsock_notify: Option<HvsockServerChannelHalf>,
    server_relay: Option<VmbusServerChannelHalf>,
    external_server: Option<mesh::Sender<InitiateContactRequest>>,
    external_requests: Option<mesh::Receiver<InitiateContactRequest>>,
    use_message_redirect: bool,
    channel_id_offset: u16,
    max_version: Option<u32>,
    delay_max_version: bool,
    enable_mnf: bool,
}

/// The server side of the connection between a vmbus server and a relay.
pub struct ServerChannelHalf<Request, Response> {
    request_send: mesh::Sender<Request>,
    response_receive: mesh::Receiver<Response>,
}

/// The relay side of a connection between a vmbus server and a relay.
pub struct RelayChannelHalf<Request, Response> {
    pub request_receive: mesh::Receiver<Request>,
    pub response_send: mesh::Sender<Response>,
}

/// A connection between a vmbus server and a relay.
pub struct RelayChannel<Request, Response> {
    pub relay_half: RelayChannelHalf<Request, Response>,
    pub server_half: ServerChannelHalf<Request, Response>,
}

impl<Request: 'static + Send, Response: 'static + Send> RelayChannel<Request, Response> {
    /// Creates a new channel between the vmbus server and a relay.
    pub fn new() -> Self {
        let (request_send, request_receive) = mesh::channel();
        let (response_send, response_receive) = mesh::channel();
        Self {
            relay_half: RelayChannelHalf {
                request_receive,
                response_send,
            },
            server_half: ServerChannelHalf {
                request_send,
                response_receive,
            },
        }
    }
}

pub type VmbusServerChannelHalf = ServerChannelHalf<ModifyRelayRequest, ModifyConnectionResponse>;
pub type VmbusRelayChannelHalf = RelayChannelHalf<ModifyRelayRequest, ModifyConnectionResponse>;
pub type VmbusRelayChannel = RelayChannel<ModifyRelayRequest, ModifyConnectionResponse>;
pub type HvsockServerChannelHalf = ServerChannelHalf<HvsockConnectRequest, HvsockConnectResult>;
pub type HvsockRelayChannelHalf = RelayChannelHalf<HvsockConnectRequest, HvsockConnectResult>;
pub type HvsockRelayChannel = RelayChannel<HvsockConnectRequest, HvsockConnectResult>;

/// A request from the server to the relay to modify connection state.
///
/// The version, use_interrupt_page and target_message_vp field can only be present if this request
/// was sent for an InitiateContact message from the guest.
#[derive(Debug, Copy, Clone)]
pub struct ModifyRelayRequest {
    pub version: Option<u32>,
    pub monitor_page: Update<MonitorPageGpas>,
    pub use_interrupt_page: Option<bool>,
}

impl From<ModifyConnectionRequest> for ModifyRelayRequest {
    fn from(value: ModifyConnectionRequest) -> Self {
        Self {
            version: value.version,
            monitor_page: value.monitor_page,
            use_interrupt_page: match value.interrupt_page {
                Update::Unchanged => None,
                Update::Reset => Some(false),
                Update::Set(_) => Some(true),
            },
        }
    }
}

#[derive(Debug)]
enum VmbusRequest {
    Reset(Rpc<(), ()>),
    Inspect(inspect::Deferred),
    Save(Rpc<(), SavedState>),
    Restore(Rpc<SavedState, Result<(), RestoreError>>),
    PostRestore(Rpc<(), Result<(), RestoreError>>),
    Start,
    Stop(Rpc<(), ()>),
}

#[derive(mesh::MeshPayload, Debug)]
pub struct OfferInfo {
    pub params: OfferParamsInternal,
    pub event: Interrupt,
    pub request_send: mesh::Sender<ChannelRequest>,
    pub server_request_recv: mesh::Receiver<ChannelServerRequest>,
}

#[derive(mesh::MeshPayload)]
pub enum OfferRequest {
    Offer(OfferInfo, mesh::OneshotSender<RemoteResult<()>>),
}

impl Inspect for VmbusServer {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.task_send.send(VmbusRequest::Inspect(req.defer()));
    }
}

struct ChannelEvent(Interrupt);

impl EventPort for ChannelEvent {
    fn handle_event(&self, _flag: u16) {
        self.0.deliver();
    }

    fn os_event(&self) -> Option<&Event> {
        self.0.event()
    }
}

#[derive(Debug, Protobuf, SavedStateRoot)]
#[mesh(package = "vmbus.server")]
pub struct SavedState {
    #[mesh(1)]
    server: channels::SavedState,
}

const MESSAGE_CONNECTION_ID: u32 = 1;
const MULTICLIENT_MESSAGE_CONNECTION_ID: u32 = 4;

impl<'a, T: Spawn> VmbusServerBuilder<'a, T> {
    /// Creates a new builder for `VmbusServer` with the default options.
    pub fn new(spawner: &'a T, synic: Arc<dyn SynicPortAccess>, gm: GuestMemory) -> Self {
        Self {
            spawner,
            synic,
            gm,
            trusted_gm: None,
            vtl: Vtl::Vtl0,
            hvsock_notify: None,
            server_relay: None,
            external_server: None,
            external_requests: None,
            use_message_redirect: false,
            channel_id_offset: 0,
            max_version: None,
            delay_max_version: false,
            enable_mnf: false,
        }
    }

    /// Sets a separate guest memory instance to use for channels that are confidential (non-relay
    /// channels in Underhill on a hardware isolated VM). This is not relevant for a non-Underhill
    /// VmBus server.
    pub fn trusted_gm(mut self, trusted_gm: Option<GuestMemory>) -> Self {
        self.trusted_gm = trusted_gm;
        self
    }

    /// Sets the VTL that this instance will serve.
    pub fn vtl(mut self, vtl: Vtl) -> Self {
        self.vtl = vtl;
        self
    }

    /// Sets a send/receive pair used to handle hvsocket requests.
    pub fn hvsock_notify(mut self, hvsock_notify: Option<HvsockServerChannelHalf>) -> Self {
        self.hvsock_notify = hvsock_notify;
        self
    }

    /// Sets a send/receive pair that will be notified of server requests. This is used by the
    /// Underhill relay.
    pub fn server_relay(mut self, server_relay: Option<VmbusServerChannelHalf>) -> Self {
        self.server_relay = server_relay;
        self
    }

    /// Sets a receiver that receives requests from another server.
    pub fn external_requests(
        mut self,
        external_requests: Option<mesh::Receiver<InitiateContactRequest>>,
    ) -> Self {
        self.external_requests = external_requests;
        self
    }

    /// Sets a sender used to forward unhandled connect requests (which used a different VTL)
    /// to another server.
    pub fn external_server(
        mut self,
        external_server: Option<mesh::Sender<InitiateContactRequest>>,
    ) -> Self {
        self.external_server = external_server;
        self
    }

    /// Sets a value which indicates whether the vmbus control plane is redirected to Underhill.
    pub fn use_message_redirect(mut self, use_message_redirect: bool) -> Self {
        self.use_message_redirect = use_message_redirect;
        self
    }

    /// Tells the server to use an offset when generating channel IDs to void collisions with
    /// another vmbus server.
    ///
    /// N.B. This should only be used by the Underhill vmbus server.
    pub fn enable_channel_id_offset(mut self, enable: bool) -> Self {
        self.channel_id_offset = if enable { 1024 } else { 0 };
        self
    }

    /// Tells the server to limit the protocol version offered to the guest.
    ///
    /// N.B. This is used for testing older protocols without requiring a specific guest OS.
    pub fn max_version(mut self, max_version: Option<u32>) -> Self {
        self.max_version = max_version;
        self
    }

    /// Delay limiting the maximum version until after the first `Unload` message.
    ///
    /// N.B. This is used to enable the use of versions older than `Version::Win10` with Uefi boot,
    ///      since that's the oldest version the Uefi client supports.
    pub fn delay_max_version(mut self, delay: bool) -> Self {
        self.delay_max_version = delay;
        self
    }

    /// Enable MNF support in the server.
    ///
    /// N.B. Enabling this has no effect if the synic does not support mapping monitor pages.
    pub fn enable_mnf(mut self, enable: bool) -> Self {
        self.enable_mnf = enable;
        self
    }

    /// Creates a new instance of the server.
    ///
    /// When the object is dropped, all channels will be closed and revoked
    /// automatically.
    pub fn build(self) -> anyhow::Result<VmbusServer> {
        #[allow(clippy::disallowed_methods)] // TODO
        let (message_send, message_recv) = mpsc::channel(64);
        let message_sender = Arc::new(MessageSender {
            send: message_send.clone(),
            multiclient: self.use_message_redirect,
        });

        let (redirect_vtl, redirect_sint) = if self.use_message_redirect {
            (REDIRECT_VTL, REDIRECT_SINT)
        } else {
            (self.vtl, SINT)
        };

        // If this server is not for VTL2, use a server-specific connection ID rather than the
        // standard one.
        let connection_id = if self.vtl == Vtl::Vtl0 && !self.use_message_redirect {
            MESSAGE_CONNECTION_ID
        } else {
            // TODO: This ID should be using the correct target VP, but that is not known until
            //       InitiateContact.
            VmbusServer::get_child_message_connection_id(0, redirect_sint, redirect_vtl)
        };

        let _message_port = self
            .synic
            .add_message_port(connection_id, redirect_vtl, message_sender)
            .context("failed to create vmbus synic ports")?;

        // If this server is for VTL0, it is also responsible for the multiclient message port.
        // N.B. If control plane redirection is enabled, the redirected message port is used for
        //      multiclient and no separate multiclient port is created.
        let _multiclient_message_port = if self.vtl == Vtl::Vtl0 && !self.use_message_redirect {
            let multiclient_message_sender = Arc::new(MessageSender {
                send: message_send,
                multiclient: true,
            });

            Some(
                self.synic
                    .add_message_port(
                        MULTICLIENT_MESSAGE_CONNECTION_ID,
                        self.vtl,
                        multiclient_message_sender,
                    )
                    .context("failed to create vmbus synic ports")?,
            )
        } else {
            None
        };

        let (offer_send, offer_recv) = mesh::mpsc_channel();
        let control = Arc::new(VmbusServerControl {
            mem: self.gm.clone(),
            trusted_mem: self.trusted_gm.clone(),
            send: offer_send,
            use_event: self.synic.prefer_os_events(),
        });

        let mut server = channels::Server::new(self.vtl, connection_id, self.channel_id_offset);

        // If requested for testing purposes, limit the maximum protocol version.
        if let Some(version) = self.max_version {
            server.set_compatibility_version(MaxVersionInfo::new(version), self.delay_max_version);
        }
        let (relay_request_send, relay_response_recv) =
            if let Some(server_relay) = self.server_relay {
                let r = server_relay.response_receive.boxed().fuse();
                (server_relay.request_send, r)
            } else {
                let (req_send, req_recv) = mesh::channel();
                let resp_recv = req_recv
                    .map(|_| {
                        ModifyConnectionResponse::Supported(
                            protocol::ConnectionState::SUCCESSFUL,
                            protocol::FeatureFlags::all(),
                        )
                    })
                    .boxed()
                    .fuse();
                (req_send, resp_recv)
            };

        // If no hvsock notifier was specified, use a default one that always sends an error response.
        let (hvsock_send, hvsock_recv) = if let Some(hvsock_notify) = self.hvsock_notify {
            let r = hvsock_notify.response_receive.boxed().fuse();
            (hvsock_notify.request_send, r)
        } else {
            let (req_send, req_recv) = mesh::channel();
            let resp_recv = req_recv
                .map(|r: HvsockConnectRequest| HvsockConnectResult::from_request(&r, false))
                .boxed()
                .fuse();
            (req_send, resp_recv)
        };

        let inner = ServerTaskInner {
            gm: self.gm,
            trusted_gm: self.trusted_gm,
            vtl: self.vtl,
            redirect_vtl,
            message_target: ConnectionTarget {
                vp: 0,
                sint: redirect_sint,
            },
            synic: self.synic,
            hvsock_requests: 0,
            hvsock_send,
            channels: HashMap::new(),
            channel_responses: FuturesUnordered::new(),
            relay_send: relay_request_send,
            external_server_send: self.external_server,
            channel_bitmap: None,
            shared_event_port: None,
            reset_done: None,
            enable_mnf: self.enable_mnf,
        };

        let (task_send, task_recv) = mesh::channel();
        let mut server_task = ServerTask {
            running: false,
            server,
            task_recv,
            offer_recv,
            message_recv,
            server_request_recv: SelectAll::new(),
            inner,
            external_requests: self.external_requests,
            next_seq: 0,
        };

        let task = self.spawner.spawn("vmbus server", async move {
            server_task.run(relay_response_recv, hvsock_recv).await;
            server_task
        });

        Ok(VmbusServer {
            task_send,
            control,
            _message_port,
            _multiclient_message_port,
            task,
        })
    }
}

impl VmbusServer {
    /// Creates a new builder for `VmbusServer` with the default options.
    pub fn builder<T: Spawn>(
        spawner: &T,
        synic: Arc<dyn SynicPortAccess>,
        gm: GuestMemory,
    ) -> VmbusServerBuilder<'_, T> {
        VmbusServerBuilder::new(spawner, synic, gm)
    }

    pub async fn save(&self) -> SavedState {
        self.task_send.call(VmbusRequest::Save, ()).await.unwrap()
    }

    pub async fn restore(&self, state: SavedState) -> Result<(), RestoreError> {
        self.task_send
            .call(VmbusRequest::Restore, state)
            .await
            .unwrap()
    }

    pub async fn post_restore(&self) -> Result<(), RestoreError> {
        self.task_send
            .call(VmbusRequest::PostRestore, ())
            .await
            .unwrap()
    }

    /// Stop the control plane.
    pub async fn stop(&self) {
        self.task_send.call(VmbusRequest::Stop, ()).await.unwrap()
    }

    /// Starts the control plane.
    pub fn start(&self) {
        self.task_send.send(VmbusRequest::Start);
    }

    /// Resets the vmbus channel state.
    pub async fn reset(&self) {
        tracing::debug!("resetting channel state");
        self.task_send.call(VmbusRequest::Reset, ()).await.unwrap()
    }

    /// Tears down the vmbus control plane.
    pub async fn shutdown(self) {
        drop(self.task_send);
        let _ = self.task.await;
    }

    #[cfg(windows)]
    pub async fn start_kernel_proxy(
        &self,
        driver: &(impl pal_async::driver::SpawnDriver + Clone),
        handle: ProxyHandle,
    ) -> Result<std::os::windows::io::OwnedHandle, std::io::Error> {
        proxyintegration::start_proxy(driver, handle, self.control(), &self.control.mem).await
    }

    /// Returns an object that can be used to offer channels.
    pub fn control(&self) -> Arc<VmbusServerControl> {
        self.control.clone()
    }

    /// Returns the message connection ID to use for a communication from the guest for servers
    /// that use a non-standard SINT or VTL.
    fn get_child_message_connection_id(vp_index: u32, sint_index: u8, vtl: Vtl) -> u32 {
        MULTICLIENT_MESSAGE_CONNECTION_ID
            | (vtl as u32) << 22
            | vp_index << 8
            | (sint_index as u32) << 4
    }
}

#[derive(mesh::MeshPayload)]
pub struct RestoreInfo {
    open_data: Option<OpenData>,
    gpadls: Vec<(GpadlId, u16, Vec<u64>)>,
    interrupt: Option<Interrupt>,
}

#[derive(Default)]
pub struct SynicMessage {
    data: Vec<u8>,
    multiclient: bool,
    trusted: bool,
}

struct ServerTask {
    running: bool,
    server: channels::Server,
    task_recv: mesh::Receiver<VmbusRequest>,
    offer_recv: mesh::MpscReceiver<OfferRequest>,
    message_recv: mpsc::Receiver<SynicMessage>,
    server_request_recv: SelectAll<TaggedStream<OfferId, mesh::Receiver<ChannelServerRequest>>>,
    inner: ServerTaskInner,
    external_requests: Option<mesh::Receiver<InitiateContactRequest>>,
    /// Next value for [`Channel::seq`].
    next_seq: u64,
}

struct ServerTaskInner {
    gm: GuestMemory,
    trusted_gm: Option<GuestMemory>,
    synic: Arc<dyn SynicPortAccess>,
    vtl: Vtl,
    redirect_vtl: Vtl,
    message_target: ConnectionTarget,
    hvsock_requests: usize,
    hvsock_send: mesh::Sender<HvsockConnectRequest>,
    channels: HashMap<OfferId, Channel>,
    channel_responses: FuturesUnordered<
        Pin<Box<dyn Send + Future<Output = (OfferId, u64, Result<ChannelResponse, RecvError>)>>>,
    >,
    external_server_send: Option<mesh::Sender<InitiateContactRequest>>,
    relay_send: mesh::Sender<ModifyRelayRequest>,
    channel_bitmap: Option<Arc<ChannelBitmap>>,
    shared_event_port: Option<Box<dyn Send>>,
    reset_done: Option<mesh::OneshotSender<()>>,
    enable_mnf: bool,
}

#[derive(Debug)]
enum ChannelResponse {
    Open(bool),
    Close,
    Gpadl(GpadlId, bool),
    TeardownGpadl(GpadlId),
    Modify(i32),
}

struct Channel {
    key: OfferKey,
    send: mesh::Sender<ChannelRequest>,
    seq: u64,
    state: ChannelState,
    gpadls: Arc<GpadlMap>,
    guest_to_host_event: Arc<ChannelEvent>,
    guest_event_port: Box<dyn GuestEventPort>,
    confidential: bool,
}

enum ChannelState {
    Closed,
    Open {
        open_params: OpenParams,
        _event_port: Box<dyn Send>,
        monitor: Option<Box<dyn Send>>,
        host_to_guest_interrupt: Interrupt,
    },
}

impl ServerTask {
    fn handle_offer(&mut self, mut info: OfferInfo) -> anyhow::Result<()> {
        let key = info.params.key();
        let confidential = info.params.flags.confidential_channel();

        // Disable mnf if the synic doesn't support it or it's not enabled in this server.
        if info.params.use_mnf
            && (!self.inner.enable_mnf || self.inner.synic.monitor_support().is_none())
        {
            info.params.use_mnf = false;
        }

        let offer_id = self
            .server
            .with_notifier(&mut self.inner)
            .offer_channel(info.params)
            .context("channel offer failed")?;

        let guest_event_port = self.inner.synic.new_guest_event_port();

        tracing::debug!(?offer_id, %key, "offered channel");

        let id = self.next_seq;
        self.next_seq += 1;
        self.inner.channels.insert(
            offer_id,
            Channel {
                key,
                send: info.request_send,
                state: ChannelState::Closed,
                gpadls: GpadlMap::new(),
                guest_to_host_event: Arc::new(ChannelEvent(info.event)),
                guest_event_port,
                seq: id,
                confidential,
            },
        );

        self.server_request_recv
            .push(TaggedStream::new(offer_id, info.server_request_recv));

        Ok(())
    }

    fn handle_revoke(&mut self, offer_id: OfferId) {
        // The channel may or may not exist in the map depending on whether it's been explicitly
        // revoked before being dropped.
        if self.inner.channels.remove(&offer_id).is_some() {
            tracing::info!(?offer_id, "revoking channel");
            self.server
                .with_notifier(&mut self.inner)
                .revoke_channel(offer_id);
        }
    }

    fn handle_response(
        &mut self,
        offer_id: OfferId,
        seq: u64,
        response: Result<ChannelResponse, RecvError>,
    ) {
        // Validate the sequence to ensure the response is not for a revoked channel.
        let channel = self
            .inner
            .channels
            .get(&offer_id)
            .filter(|channel| channel.seq == seq);

        if let Some(channel) = channel {
            match response {
                Ok(response) => match response {
                    ChannelResponse::Open(ok) => self.handle_open(offer_id, ok),
                    ChannelResponse::Close => self.handle_close(offer_id),
                    ChannelResponse::Gpadl(gpadl_id, ok) => {
                        self.handle_gpadl_create(offer_id, gpadl_id, ok)
                    }
                    ChannelResponse::TeardownGpadl(gpadl_id) => {
                        self.handle_gpadl_teardown(offer_id, gpadl_id)
                    }
                    ChannelResponse::Modify(status) => self.handle_modify_channel(offer_id, status),
                },
                Err(err) => {
                    tracing::error!(
                        key = %channel.key,
                        error = &err as &dyn std::error::Error,
                        "channel response failure, channel is in inconsistent state until revoked"
                    );
                }
            }
        } else {
            tracing::debug!(offer_id = ?offer_id, seq, ?response, "received response after revoke");
        }
    }

    fn handle_open(&mut self, offer_id: OfferId, ok: bool) {
        let status = if ok {
            0
        } else {
            let channel = self
                .inner
                .channels
                .get_mut(&offer_id)
                .expect("channel still exists");
            channel.state = ChannelState::Closed;
            protocol::STATUS_UNSUCCESSFUL
        };
        self.server
            .with_notifier(&mut self.inner)
            .open_complete(offer_id, status);
    }

    fn handle_close(&mut self, offer_id: OfferId) {
        let channel = self
            .inner
            .channels
            .get_mut(&offer_id)
            .expect("channel still exists");
        channel.state = ChannelState::Closed;
        self.server
            .with_notifier(&mut self.inner)
            .close_complete(offer_id);
    }

    fn handle_gpadl_create(&mut self, offer_id: OfferId, gpadl_id: GpadlId, ok: bool) {
        let status = if ok { 0 } else { protocol::STATUS_UNSUCCESSFUL };
        self.server
            .with_notifier(&mut self.inner)
            .gpadl_create_complete(offer_id, gpadl_id, status);
    }

    fn handle_gpadl_teardown(&mut self, offer_id: OfferId, gpadl_id: GpadlId) {
        self.server
            .with_notifier(&mut self.inner)
            .gpadl_teardown_complete(offer_id, gpadl_id);
    }

    fn handle_modify_channel(&mut self, offer_id: OfferId, status: i32) {
        self.server
            .with_notifier(&mut self.inner)
            .modify_channel_complete(offer_id, status);
    }

    fn handle_restore_channel(
        &mut self,
        offer_id: OfferId,
        open: bool,
    ) -> anyhow::Result<RestoreResult> {
        let gpadls = self.server.channel_gpadls(offer_id);
        let params = self
            .server
            .with_notifier(&mut self.inner)
            .restore_channel(offer_id, open)?;

        let channel = self.inner.channels.get_mut(&offer_id).unwrap();
        for gpadl in &gpadls {
            if let Ok(buf) =
                MultiPagedRangeBuf::new(gpadl.request.count.into(), gpadl.request.buf.clone())
            {
                channel.gpadls.add(gpadl.request.id, buf);
            }
        }

        let open_request = params.map(|params| {
            let (_, interrupt) = self.inner.open_channel(offer_id, &params);
            OpenRequest {
                open_data: params.open_data,
                interrupt,
            }
        });
        let result = RestoreResult {
            open_request,
            gpadls,
        };
        Ok(result)
    }

    fn handle_request(&mut self, request: VmbusRequest) {
        tracing::debug!(?request, "handle_request");
        match request {
            VmbusRequest::Reset(Rpc((), done)) => {
                assert!(self.inner.reset_done.is_none());
                self.inner.reset_done = Some(done);
                self.server.with_notifier(&mut self.inner).reset();
                // TODO: clear pending messages and other requests.
            }
            VmbusRequest::Inspect(deferred) => {
                deferred.respond(|resp| {
                    resp.field("message_target.vp", self.inner.message_target.vp)
                        .field("running", self.running)
                        .field("hvsock_requests", self.inner.hvsock_requests)
                        .field_mut_with("unstick_channels", |v| {
                            let v: inspect::Value = if let Some(v) = v {
                                if v == "force" {
                                    self.unstick_channels(true);
                                    v.into()
                                } else {
                                    let v =
                                        v.parse().ok().context("expected false, true, or force")?;
                                    if v {
                                        self.unstick_channels(false);
                                    }
                                    v.into()
                                }
                            } else {
                                false.into()
                            };
                            anyhow::Ok(v)
                        })
                        .merge(&self.server.with_notifier(&mut self.inner));
                });
            }
            VmbusRequest::Save(rpc) => rpc.handle_sync(|()| SavedState {
                server: self.server.save(),
            }),
            VmbusRequest::Restore(rpc) => {
                rpc.handle_sync(|state| self.server.restore(state.server))
            }
            VmbusRequest::PostRestore(rpc) => {
                rpc.handle_sync(|()| self.server.with_notifier(&mut self.inner).post_restore())
            }
            VmbusRequest::Stop(rpc) => rpc.handle_sync(|()| {
                if self.running {
                    self.running = false;
                }
            }),
            VmbusRequest::Start => {
                if !self.running {
                    self.running = true;
                }
            }
        }
    }

    fn handle_relay_response(&mut self, response: ModifyConnectionResponse) {
        self.server
            .with_notifier(&mut self.inner)
            .complete_modify_connection(response);
    }

    fn handle_tl_connect_result(&mut self, result: HvsockConnectResult) {
        assert_ne!(self.inner.hvsock_requests, 0);
        self.inner.hvsock_requests -= 1;

        self.server
            .with_notifier(&mut self.inner)
            .send_tl_connect_result(result);
    }

    fn handle_synic_message(&mut self, message: SynicMessage) {
        match self
            .server
            .with_notifier(&mut self.inner)
            .handle_synic_message(message)
        {
            Ok(()) => {}
            Err(err) => {
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    "synic message error"
                );
            }
        }
    }

    /// Handles a request forwarded by a different vmbus server. This is used to forward requests
    /// for different VTLs to different servers.
    ///
    /// N.B. This uses the same mechanism as the HCL server relay, so all requests, even the ones
    ///      meant for the primary server, are forwarded. In that case the primary server depends
    ///      on this server to send back a response so it can continue handling it.
    fn handle_external_request(&mut self, request: InitiateContactRequest) {
        self.server
            .with_notifier(&mut self.inner)
            .initiate_contact(request);
    }

    async fn run(
        &mut self,
        mut relay_response_recv: impl futures::stream::FusedStream<Item = ModifyConnectionResponse>
            + Unpin,
        mut hvsock_recv: impl futures::stream::FusedStream<Item = HvsockConnectResult> + Unpin,
    ) {
        loop {
            // Create an OptionFuture for each event that should only be handled
            // while the VM is running. In other cases, leave the events in
            // their respective queues.

            let mut external_requests = OptionFuture::from(
                self.running
                    .then(|| {
                        self.external_requests
                            .as_mut()
                            .map(|r| r.select_next_some())
                    })
                    .flatten(),
            );

            // Only handle new messages if there are not too many hvsock
            // requests outstanding. This puts a bound on the resources used by
            // the guest.
            let mut message_recv = OptionFuture::from(
                (self.running && self.inner.hvsock_requests < MAX_CONCURRENT_HVSOCK_REQUESTS)
                    .then(|| self.message_recv.select_next_some()),
            );

            // Accept channel responses until stopped or when resetting.
            let mut channel_response = OptionFuture::from(
                (self.running || self.inner.reset_done.is_some())
                    .then(|| self.inner.channel_responses.select_next_some()),
            );

            // Accept hvsock connect responses while the VM is running.
            let mut hvsock_response =
                OptionFuture::from(self.running.then(|| hvsock_recv.select_next_some()));

            futures::select! { // merge semantics
                r = self.task_recv.recv().fuse() => {
                    if let Ok(request) = r {
                        self.handle_request(request);
                    } else {
                        break;
                    }
                }
                r = self.offer_recv.select_next_some() => {
                    match r {
                        OfferRequest::Offer(request, response) => {
                            response.send(self.handle_offer(request).map_err(RemoteError::new))
                        },
                    }
                }
                r = self.server_request_recv.select_next_some() => {
                    match r {
                        (id, Some(request)) => match request {
                            ChannelServerRequest::Restore(rpc) => rpc.handle_failable_sync(|open| {
                                self.handle_restore_channel(id, open)
                            }),
                            ChannelServerRequest::Revoke(rpc) => rpc.handle_sync(|_| {
                                self.handle_revoke(id);
                            })
                        },
                        (id, None) => self.handle_revoke(id),
                    }
                }
                r = channel_response => {
                    let (id, seq, response) = r.unwrap();
                    self.handle_response(id, seq, response);
                }
                r = relay_response_recv.select_next_some() => {
                    self.handle_relay_response(r);
                },
                r = hvsock_response => {
                    self.handle_tl_connect_result(r.unwrap());
                }
                data = message_recv => {
                    let data = data.unwrap();
                    self.handle_synic_message(data);
                }
                r = external_requests => {
                    let r = r.unwrap();
                    self.handle_external_request(r);
                }
                complete => break,
            }
        }
    }

    /// Wakes the host and guest for every open channel. If `force`, always
    /// wakes both the host and guest. If `!force`, only wake for rings that are
    /// in the state where a notification is expected.
    fn unstick_channels(&self, force: bool) {
        for channel in self.inner.channels.values() {
            if let Err(err) = self.unstick_channel(channel, force) {
                tracing::warn!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick channel"
                );
            }
        }
    }

    fn unstick_channel(&self, channel: &Channel, force: bool) -> anyhow::Result<()> {
        if let ChannelState::Open {
            open_params,
            host_to_guest_interrupt,
            ..
        } = &channel.state
        {
            if force {
                tracing::info!(channel = %channel.key, "waking host and guest");
                channel.guest_to_host_event.0.deliver();
                host_to_guest_interrupt.deliver();
                return Ok(());
            }

            let gpadl = channel
                .gpadls
                .clone()
                .view()
                .map(open_params.open_data.ring_gpadl_id)
                .context("couldn't find ring gpadl")?;

            let aligned = AlignedGpadlView::new(gpadl)
                .ok()
                .context("ring not aligned")?;
            let (in_gpadl, out_gpadl) = aligned
                .split(open_params.open_data.ring_offset)
                .ok()
                .context("couldn't split ring")?;

            if let Err(err) = self.unstick_incoming_ring(channel, in_gpadl, host_to_guest_interrupt)
            {
                tracing::warn!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick incoming ring"
                );
            }
            if let Err(err) =
                self.unstick_outgoing_ring(channel, out_gpadl, host_to_guest_interrupt)
            {
                tracing::warn!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick outgoing ring"
                );
            }
        }
        Ok(())
    }

    fn unstick_incoming_ring(
        &self,
        channel: &Channel,
        in_gpadl: AlignedGpadlView,
        host_to_guest_interrupt: &Interrupt,
    ) -> Result<(), anyhow::Error> {
        let incoming_mem = GpadlRingMem::new(in_gpadl, &self.inner.gm)?;
        if ring::reader_needs_signal(&incoming_mem) {
            tracing::info!(channel = %channel.key, "waking host for incoming ring");
            channel.guest_to_host_event.0.deliver();
        }
        if ring::writer_needs_signal(&incoming_mem) {
            tracing::info!(channel = %channel.key, "waking guest for incoming ring");
            host_to_guest_interrupt.deliver();
        }
        Ok(())
    }

    fn unstick_outgoing_ring(
        &self,
        channel: &Channel,
        out_gpadl: AlignedGpadlView,
        host_to_guest_interrupt: &Interrupt,
    ) -> Result<(), anyhow::Error> {
        let outgoing_mem = GpadlRingMem::new(out_gpadl, &self.inner.gm)?;
        if ring::reader_needs_signal(&outgoing_mem) {
            tracing::info!(channel = %channel.key, "waking guest for outgoing ring");
            host_to_guest_interrupt.deliver();
        }
        if ring::writer_needs_signal(&outgoing_mem) {
            tracing::info!(channel = %channel.key, "waking host for outgoing ring");
            channel.guest_to_host_event.0.deliver();
        }
        Ok(())
    }
}

impl channels::Notifier for ServerTaskInner {
    fn notify(&mut self, offer_id: OfferId, action: channels::Action) {
        let channel = self
            .channels
            .get_mut(&offer_id)
            .expect("channel does not exist");

        fn handle<I: 'static + Send, R: 'static + Send>(
            offer_id: OfferId,
            channel: &Channel,
            req: impl FnOnce(Rpc<I, R>) -> ChannelRequest,
            input: I,
            f: impl 'static + Send + FnOnce(R) -> ChannelResponse,
        ) -> Pin<Box<dyn Send + Future<Output = (OfferId, u64, Result<ChannelResponse, RecvError>)>>>
        {
            let (response, recv) = mesh::oneshot();
            channel.send.send((req)(Rpc(input, response)));
            let seq = channel.seq;
            Box::pin(async move {
                let r = recv.await.map(f);
                (offer_id, seq, r)
            })
        }

        let response = match action {
            channels::Action::Open(open_params) => {
                let (channel, interrupt) = self.open_channel(offer_id, &open_params);
                handle(
                    offer_id,
                    channel,
                    ChannelRequest::Open,
                    OpenRequest {
                        open_data: open_params.open_data,
                        interrupt,
                    },
                    ChannelResponse::Open,
                )
            }
            channels::Action::Close => {
                if let Some(channel_bitmap) = self.channel_bitmap.as_ref() {
                    if let ChannelState::Open { open_params, .. } = channel.state {
                        channel_bitmap.unregister_channel(open_params.event_flag);
                    }
                }

                channel.guest_event_port.clear();
                handle(offer_id, channel, ChannelRequest::Close, (), |()| {
                    ChannelResponse::Close
                })
            }
            channels::Action::Gpadl(gpadl_id, count, buf) => {
                channel.gpadls.add(
                    gpadl_id,
                    MultiPagedRangeBuf::new(count.into(), buf.clone()).unwrap(),
                );
                handle(
                    offer_id,
                    channel,
                    ChannelRequest::Gpadl,
                    GpadlRequest {
                        id: gpadl_id,
                        count,
                        buf,
                    },
                    move |r| ChannelResponse::Gpadl(gpadl_id, r),
                )
            }
            channels::Action::TeardownGpadl {
                gpadl_id,
                post_restore,
            } => {
                if !post_restore {
                    channel.gpadls.remove(gpadl_id, Box::new(|| ()));
                }

                handle(
                    offer_id,
                    channel,
                    ChannelRequest::TeardownGpadl,
                    gpadl_id,
                    move |()| ChannelResponse::TeardownGpadl(gpadl_id),
                )
            }
            channels::Action::Modify { target_vp } => {
                if let ChannelState::Open { open_params, .. } = channel.state {
                    let (target_vtl, target_sint) = if open_params.flags.redirect_interrupt() {
                        (self.redirect_vtl, self.message_target.sint)
                    } else {
                        (self.vtl, SINT)
                    };

                    channel.guest_event_port.set(
                        target_vtl,
                        target_vp,
                        target_sint,
                        open_params.event_flag,
                    );

                    handle(
                        offer_id,
                        channel,
                        ChannelRequest::Modify,
                        ModifyRequest::TargetVp { target_vp },
                        ChannelResponse::Modify,
                    )
                } else {
                    unreachable!();
                }
            }
        };
        self.channel_responses.push(response);
    }

    fn modify_connection(&mut self, mut request: ModifyConnectionRequest) -> anyhow::Result<()> {
        self.map_interrupt_page(request.interrupt_page)
            .context("Failed to map interrupt page.")?;

        self.set_monitor_page(request.monitor_page, request.force)
            .context("Failed to map monitor page.")?;

        if let Some(vp) = request.target_message_vp {
            self.message_target.vp = vp;
        }

        if request.notify_relay {
            // If this server is handling MNF, the monitor pages should not be relayed.
            // N.B. Since the relay is being asked not to update the monitor pages, rather than
            //      reset them, this is only safe because the value of enable_mnf won't change after
            //      the server has been created.
            if self.enable_mnf {
                request.monitor_page = Update::Unchanged;
            }

            self.relay_send.send(request.into());
        }

        Ok(())
    }

    fn forward_unhandled(&mut self, request: InitiateContactRequest) {
        if let Some(external_server) = &self.external_server_send {
            external_server.send(request);
        } else {
            tracing::warn!(?request, "nowhere to forward unhandled request")
        }
    }

    fn inspect(&self, offer_id: OfferId, req: inspect::Request<'_>) {
        let channel = self.channels.get(&offer_id).expect("should exist");
        let mut resp = req.respond();
        if let ChannelState::Open { open_params, .. } = &channel.state {
            let mem = if channel.confidential && self.trusted_gm.is_some() {
                self.trusted_gm.as_ref().unwrap()
            } else {
                &self.gm
            };

            inspect_rings(
                &mut resp,
                mem,
                channel.gpadls.clone(),
                &open_params.open_data,
            );
        }
    }

    fn send_message(&mut self, message: OutgoingMessage, target: Option<ConnectionTarget>) {
        let ConnectionTarget { vp, sint } = target.unwrap_or(self.message_target);

        self.synic
            .post_message(self.redirect_vtl, vp, sint, 1, message.data());
    }

    fn notify_hvsock(&mut self, request: &HvsockConnectRequest) {
        self.hvsock_requests += 1;
        self.hvsock_send.send(*request);
    }

    fn reset_complete(&mut self) {
        if let Some(monitor) = self.synic.monitor_support() {
            if let Err(err) = monitor.set_monitor_page(None) {
                tracing::warn!(?err, "resetting monitor page failed")
            }
        }

        let done = self.reset_done.take().expect("must have requested reset");
        done.send(());
    }
}

impl ServerTaskInner {
    fn open_channel(
        &mut self,
        offer_id: OfferId,
        open_params: &OpenParams,
    ) -> (&mut Channel, Interrupt) {
        let channel = self
            .channels
            .get_mut(&offer_id)
            .expect("channel does not exist");

        // Always register with the channel bitmap; if Win7, this may be unnecessary.
        if let Some(channel_bitmap) = self.channel_bitmap.as_ref() {
            channel_bitmap.register_channel(
                open_params.event_flag,
                channel.guest_to_host_event.0.clone(),
            );
        }
        // Always set up an event port; if V1, this will be unused.
        let event_port = self
            .synic
            .add_event_port(
                open_params.connection_id,
                self.vtl,
                channel.guest_to_host_event.clone(),
            )
            .expect("connection ID should not be in use");
        // For pre-Win8 guests, the host-to-guest event always targets vp 0 and the channel
        // bitmap is used instead of the event flag.
        let (target_vp, event_flag) = if self.channel_bitmap.is_some() {
            (0, 0)
        } else {
            (open_params.open_data.target_vp, open_params.event_flag)
        };
        let (target_vtl, target_sint) = if open_params.flags.redirect_interrupt() {
            (self.redirect_vtl, self.message_target.sint)
        } else {
            (self.vtl, SINT)
        };
        channel
            .guest_event_port
            .set(target_vtl, target_vp, target_sint, event_flag);
        let interrupt = ChannelBitmap::create_interrupt(
            &self.channel_bitmap,
            channel.guest_event_port.interrupt(),
            open_params.event_flag,
        );

        let monitor = open_params.monitor_id.and_then(|monitor_id| {
            self.synic
                .monitor_support()
                .map(|monitor| monitor.register_monitor(monitor_id, open_params.connection_id))
        });

        channel.state = ChannelState::Open {
            open_params: *open_params,
            _event_port: event_port,
            monitor,
            host_to_guest_interrupt: interrupt.clone(),
        };
        (channel, interrupt)
    }

    /// If the client specified an interrupt page, map it into host memory and
    /// set up the shared event port.
    fn map_interrupt_page(
        &mut self,
        interrupt_page: Update<u64>,
    ) -> Result<(), InterruptPageError> {
        let interrupt_page = match interrupt_page {
            Update::Unchanged => return Ok(()),
            Update::Reset => {
                self.channel_bitmap = None;
                self.shared_event_port = None;
                return Ok(());
            }
            Update::Set(interrupt_page) => interrupt_page,
        };

        assert_ne!(interrupt_page, 0);

        if interrupt_page % PAGE_SIZE as u64 != 0 {
            return Err(InterruptPageError::NotPageAligned(interrupt_page));
        }

        let interrupt_page = self
            .gm
            .lock_gpns(false, &[interrupt_page / PAGE_SIZE as u64])?;
        let channel_bitmap = Arc::new(ChannelBitmap::new(interrupt_page));
        self.channel_bitmap = Some(channel_bitmap.clone());

        // Create the shared event port for pre-Win8 guests.
        let interrupt = Interrupt::from_fn(move || {
            channel_bitmap.handle_shared_interrupt();
        });

        self.shared_event_port = Some(self.synic.add_event_port(
            SHARED_EVENT_CONNECTION_ID,
            self.vtl,
            Arc::new(ChannelEvent(interrupt)),
        )?);

        Ok(())
    }

    fn set_monitor_page(
        &mut self,
        monitor_page: Update<MonitorPageGpas>,
        force: bool,
    ) -> anyhow::Result<()> {
        let monitor_page = match monitor_page {
            Update::Unchanged => return Ok(()),
            Update::Reset => None,
            Update::Set(value) => Some(value),
        };

        // Force is used by restore because there may be restored channels in the open state.
        if !force
            && self.channels.iter().any(|(_, c)| {
                matches!(
                    &c.state,
                    ChannelState::Open {
                        monitor: Some(_),
                        ..
                    }
                )
            })
        {
            anyhow::bail!("attempt to change monitor page while open channels using mnf");
        }

        if self.enable_mnf {
            if let Some(monitor) = self.synic.monitor_support() {
                if let Err(err) =
                    monitor.set_monitor_page(monitor_page.map(|mp| mp.child_to_parent))
                {
                    anyhow::bail!(
                        "setting monitor page failed, err = {err:?}, monitor_page = {monitor_page:?}"
                    );
                }
            }
        }

        Ok(())
    }
}

/// Control point for [`VmbusServer`], allowing callers to offer channels.
#[derive(Clone)]
pub struct VmbusServerControl {
    mem: GuestMemory,
    trusted_mem: Option<GuestMemory>,
    send: mesh::MpscSender<OfferRequest>,
    use_event: bool,
}

impl VmbusServerControl {
    /// Offers a channel to the vmbus server, where the flags and user_defined data are already set.
    /// This is used by the relay to forward the host's parameters.
    pub async fn offer_core(&self, offer_info: OfferInfo) -> anyhow::Result<OfferResources> {
        let confidential = offer_info.params.flags.confidential_channel();
        let (send, recv) = mesh::oneshot();
        self.send.send(OfferRequest::Offer(offer_info, send));
        recv.await.flatten()?;
        Ok(OfferResources {
            guest_mem: if confidential && self.trusted_mem.is_some() {
                self.trusted_mem.as_ref().unwrap().clone()
            } else {
                self.mem.clone()
            },
        })
    }

    async fn offer(&self, request: OfferInput) -> anyhow::Result<OfferResources> {
        let offer_info = OfferInfo {
            params: request.params.into(),
            event: request.event,
            request_send: request.request_send,
            server_request_recv: request.server_request_recv,
        };

        self.offer_core(offer_info).await
    }
}

/// Inspects the specified ring buffer state by directly accessing guest memory.
fn inspect_rings(
    resp: &mut inspect::Response<'_>,
    gm: &GuestMemory,
    gpadl_map: Arc<GpadlMap>,
    open_data: &OpenData,
) -> Option<()> {
    let gpadl = gpadl_map
        .view()
        .map(GpadlId(open_data.ring_gpadl_id.0))
        .ok()?;
    let aligned = AlignedGpadlView::new(gpadl).ok()?;
    let (in_gpadl, out_gpadl) = aligned.split(open_data.ring_offset).ok()?;
    if let Ok(incoming_mem) = GpadlRingMem::new(in_gpadl, gm) {
        resp.child("incoming_ring", |req| ring::inspect_ring(incoming_mem, req));
    }
    if let Ok(outgoing_mem) = GpadlRingMem::new(out_gpadl, gm) {
        resp.child("outgoing_ring", |req| ring::inspect_ring(outgoing_mem, req));
    }
    Some(())
}

pub(crate) struct MessageSender {
    send: mpsc::Sender<SynicMessage>,
    multiclient: bool,
}

impl MessagePort for MessageSender {
    fn handle_message(&self, data: &[u8], trusted: bool) -> bool {
        self.send
            .clone()
            .try_send(SynicMessage {
                data: data.to_vec(),
                multiclient: self.multiclient,
                trusted,
            })
            .is_ok()
    }
}

#[async_trait]
impl ParentBus for VmbusServerControl {
    async fn add_child(&self, request: OfferInput) -> anyhow::Result<OfferResources> {
        self.offer(request).await
    }

    fn clone_bus(&self) -> Box<dyn ParentBus> {
        Box::new(self.clone())
    }

    fn use_event(&self) -> bool {
        self.use_event
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use vmbus_channel::bus::OfferParams;
    use vmbus_core::protocol::ChannelId;
    use vmbus_core::protocol::VmbusMessage;
    use vmcore::synic::SynicPortAccess;
    use zerocopy::AsBytes;
    use zerocopy::FromBytes;

    struct MockSynicInner {
        message_port: Option<Arc<dyn MessagePort>>,
    }

    struct MockSynic {
        inner: Mutex<MockSynicInner>,
        message_send: mesh::Sender<Vec<u8>>,
    }

    impl MockSynic {
        fn new(message_send: mesh::Sender<Vec<u8>>) -> Self {
            Self {
                inner: Mutex::new(MockSynicInner { message_port: None }),
                message_send,
            }
        }

        fn send_message(&self, msg: impl VmbusMessage + AsBytes) {
            self.send_message_core(OutgoingMessage::new(&msg), false);
        }

        fn send_message_trusted(&self, msg: impl VmbusMessage + AsBytes) {
            self.send_message_core(OutgoingMessage::new(&msg), true);
        }

        fn send_message_with_data(&self, msg: impl VmbusMessage + AsBytes, data: &[u8]) {
            self.send_message_core(OutgoingMessage::with_data(&msg, data), false);
        }

        fn send_message_core(&self, msg: OutgoingMessage, trusted: bool) {
            self.inner
                .lock()
                .message_port
                .as_ref()
                .unwrap()
                .handle_message(msg.data(), trusted);
        }
    }

    #[derive(Debug)]
    struct MockGuestPort {}

    impl GuestEventPort for MockGuestPort {
        fn interrupt(&self) -> Interrupt {
            todo!()
        }

        fn clear(&mut self) {
            todo!()
        }

        fn set(&mut self, _vtl: Vtl, _vp: u32, _sint: u8, _flag: u16) {
            todo!()
        }
    }

    impl SynicPortAccess for MockSynic {
        fn add_message_port(
            &self,
            connection_id: u32,
            _minimum_vtl: Vtl,
            port: Arc<dyn MessagePort>,
        ) -> Result<Box<dyn Sync + Send>, vmcore::synic::Error> {
            self.inner.lock().message_port = Some(port);
            Ok(Box::new(connection_id))
        }

        fn add_event_port(
            &self,
            connection_id: u32,
            _minimum_vtl: Vtl,
            _port: Arc<dyn EventPort>,
        ) -> Result<Box<dyn Sync + Send>, vmcore::synic::Error> {
            Ok(Box::new(connection_id))
        }

        fn post_message(&self, _vtl: Vtl, _vp: u32, _sint: u8, _typ: u32, payload: &[u8]) {
            self.message_send.send(payload.into());
        }

        fn new_guest_event_port(&self) -> Box<dyn GuestEventPort> {
            Box::new(MockGuestPort {})
        }

        fn prefer_os_events(&self) -> bool {
            false
        }
    }

    struct TestChannel {
        request_recv: mesh::Receiver<ChannelRequest>,
        server_request_send: mesh::Sender<ChannelServerRequest>,
        _resources: OfferResources,
    }

    impl TestChannel {
        async fn next_request(&mut self) -> ChannelRequest {
            self.request_recv.next().await.unwrap()
        }

        async fn handle_gpadl(&mut self) {
            let ChannelRequest::Gpadl(rpc) = self.next_request().await else {
                panic!("Wrong request");
            };

            rpc.complete(true);
        }

        async fn handle_gpadl_teardown(&mut self) {
            let rpc = self.get_gpadl_teardown().await;
            rpc.complete(());
        }

        async fn get_gpadl_teardown(&mut self) -> Rpc<GpadlId, ()> {
            let ChannelRequest::TeardownGpadl(rpc) = self.next_request().await else {
                panic!("Wrong request");
            };

            rpc
        }

        async fn restore(&self) {
            self.server_request_send
                .call(ChannelServerRequest::Restore, false)
                .await
                .unwrap()
                .unwrap();
        }
    }

    struct TestEnv {
        vmbus: VmbusServer,
        synic: Arc<MockSynic>,
        message_recv: mesh::Receiver<Vec<u8>>,
    }

    impl TestEnv {
        fn new(spawner: impl Spawn) -> Self {
            let (message_send, message_recv) = mesh::channel();
            let synic = Arc::new(MockSynic::new(message_send));
            let gm = GuestMemory::empty();
            let vmbus = VmbusServerBuilder::new(&spawner, synic.clone(), gm)
                .build()
                .unwrap();

            Self {
                vmbus,
                synic,
                message_recv,
            }
        }

        async fn offer(&self) -> TestChannel {
            let (request_send, request_recv) = mesh::channel();
            let (server_request_send, server_request_recv) = mesh::channel();
            let offer = OfferInput {
                event: Interrupt::from_fn(|| {}),
                request_send,
                server_request_recv,
                params: OfferParams {
                    interface_name: "test".into(),
                    instance_id: Guid::ZERO,
                    interface_id: Guid::ZERO,
                    mmio_megabytes: 0,
                    mmio_megabytes_optional: 0,
                    channel_type: vmbus_channel::bus::ChannelType::Device {
                        pipe_packets: false,
                    },
                    subchannel_index: 0,
                    use_mnf: false,
                    offer_order: None,
                },
            };

            let control = self.vmbus.control();
            let _resources = control.add_child(offer).await.unwrap();

            TestChannel {
                request_recv,
                server_request_send,
                _resources,
            }
        }

        async fn expect_response(&mut self, expected: protocol::MessageType) {
            let data = self.message_recv.next().await.unwrap();
            let header = protocol::MessageHeader::read_from_prefix(&data).unwrap();

            assert_eq!(expected, header.message_type())
        }

        async fn get_response<T: VmbusMessage + FromBytes>(&mut self) -> T {
            use zerocopy_helpers::FromBytesExt;
            let data = self.message_recv.next().await.unwrap();
            let (header, message) = protocol::MessageHeader::read_from_prefix_split(&data).unwrap();
            assert_eq!(T::MESSAGE_TYPE, header.message_type());
            T::read_from_prefix(message).unwrap()
        }

        fn initiate_contact(
            &self,
            version: protocol::Version,
            feature_flags: protocol::FeatureFlags,
            trusted: bool,
        ) {
            self.synic.send_message_core(
                OutgoingMessage::new(&protocol::InitiateContact {
                    version_requested: version as u32,
                    target_message_vp: 0,
                    child_to_parent_monitor_page_gpa: 0,
                    parent_to_child_monitor_page_gpa: 0,
                    interrupt_page_or_target_info: *protocol::TargetInfo::new(2, 0, feature_flags)
                        .as_u64(),
                }),
                trusted,
            );
        }

        async fn connect(&mut self, offer_count: u32) {
            self.initiate_contact(
                protocol::Version::Win10,
                protocol::FeatureFlags::new(),
                false,
            );

            self.expect_response(protocol::MessageType::VERSION_RESPONSE)
                .await;

            self.synic.send_message(protocol::RequestOffers {});

            for _ in 0..offer_count {
                self.expect_response(protocol::MessageType::OFFER_CHANNEL)
                    .await;
            }

            self.expect_response(protocol::MessageType::ALL_OFFERS_DELIVERED)
                .await;
        }
    }

    #[async_test]
    async fn test_save_restore(spawner: impl Spawn) {
        // Most save/restore state is tested in mod channels::tests; this test specifically checks
        // that ServerTaskInner correctly handles some aspects of the save/restore.
        //
        // If this test fails, it is more likely to hang than panic.
        let mut env = TestEnv::new(spawner);
        let mut channel = env.offer().await;
        env.vmbus.start();
        env.connect(1).await;

        // Create a GPADL for the channel.
        env.synic.send_message_with_data(
            protocol::GpadlHeader {
                channel_id: ChannelId(1),
                gpadl_id: GpadlId(10),
                count: 1,
                len: 16,
            },
            [1u64, 0u64].as_bytes(),
        );

        channel.handle_gpadl().await;
        env.expect_response(protocol::MessageType::GPADL_CREATED)
            .await;

        // Start tearing it down.
        env.synic.send_message(protocol::GpadlTeardown {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(10),
        });

        // Wait for the teardown request here to make sure the server has processed the teardown
        // message, but do not complete it before saving.
        let rpc = channel.get_gpadl_teardown().await;
        env.vmbus.stop().await;
        let saved_state = env.vmbus.save().await;
        env.vmbus.start();

        // Finish tearing down the gpadl and release the channel so the server can reset.
        rpc.complete(());
        env.expect_response(protocol::MessageType::GPADL_TORNDOWN)
            .await;

        env.synic.send_message(protocol::RelIdReleased {
            channel_id: ChannelId(1),
        });

        env.vmbus.reset().await;
        env.vmbus.stop().await;

        // When restoring with a gpadl in the TearingDown state, the teardown request for the device
        // will be repeated. This must not panic.
        env.vmbus.restore(saved_state).await.unwrap();
        channel.restore().await;
        env.vmbus.post_restore().await.unwrap();
        env.vmbus.start();

        // Handle the teardown after restore.
        channel.handle_gpadl_teardown().await;
        env.expect_response(protocol::MessageType::GPADL_TORNDOWN)
            .await;

        env.synic.send_message(protocol::RelIdReleased {
            channel_id: ChannelId(1),
        });
    }

    #[async_test]
    async fn test_confidential_connection(spawner: impl Spawn) {
        let mut env = TestEnv::new(spawner);
        // Add a regular bus child channel.
        let _channel = env.offer().await;

        // Add a channel directly, like the relay would do.
        let (request_send, _request_recv) = mesh::channel();
        let (_server_request_send, server_request_recv) = mesh::channel();
        let mut id = Guid::ZERO;
        id.data1 = 1;
        let control = env.vmbus.control();
        let _relay_channel = control
            .offer_core(OfferInfo {
                params: OfferParamsInternal {
                    interface_name: "test".into(),
                    instance_id: id,
                    interface_id: id,
                    mmio_megabytes: 0,
                    mmio_megabytes_optional: 0,
                    subchannel_index: 0,
                    use_mnf: false,
                    offer_order: None,
                    flags: protocol::OfferFlags::new().with_enumerate_device_interface(true),
                    ..Default::default()
                },
                event: Interrupt::from_fn(|| {}),
                request_send,
                server_request_recv,
            })
            .await
            .unwrap();

        env.vmbus.start();
        env.initiate_contact(
            protocol::Version::Copper,
            protocol::FeatureFlags::new().with_confidential_channels(true),
            true,
        );

        env.expect_response(protocol::MessageType::VERSION_RESPONSE)
            .await;

        env.synic.send_message_trusted(protocol::RequestOffers {});

        // All offers added with add_child are confidential.
        let offer = env.get_response::<protocol::OfferChannel>().await;
        assert!(offer.flags.confidential_channel());

        // The "relay" channel will not have its flags modified.
        let offer = env.get_response::<protocol::OfferChannel>().await;
        assert!(!offer.flags.confidential_channel());

        env.expect_response(protocol::MessageType::ALL_OFFERS_DELIVERED)
            .await;
    }
}