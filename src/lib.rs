//! # Control Interface Client
//!
//! This library provides a client API for consuming the wasmCloud control interface over a
//! NATS connection. This library can be used by multiple types of tools, and is also used
//! by the control interface capability provider and the wash CLI

mod broker;
mod kv;
mod sub_stream;
mod types;

pub use types::*;

use async_nats::jetstream::kv::Store;
use cloudevents::event::Event;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};
use sub_stream::collect_timeout;
use tokio::sync::mpsc::Receiver;
use tracing::{debug, error, instrument, trace};
use tracing_futures::Instrument;
use wasmbus_rpc::core::LinkDefinition;
use wasmbus_rpc::otel::OtelHeaderInjector;

type Result<T> = ::std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Lattice control interface client
#[derive(Clone, Debug)]
pub struct Client {
    nc: async_nats::Client,
    topic_prefix: Option<String>,
    ns_prefix: String,
    timeout: Duration,
    auction_timeout: Duration,
    kvstore: Option<Store>,
}

/// A client builder that can be used to fluently provide configuration settings used to construct
/// the control interface client
pub struct ClientBuilder {
    nc: Option<async_nats::Client>,
    topic_prefix: Option<String>,
    ns_prefix: String,
    timeout: Duration,
    auction_timeout: Duration,
    js_domain: Option<String>,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            nc: None,
            topic_prefix: None,
            ns_prefix: "default".to_string(),
            timeout: Duration::from_secs(2),
            auction_timeout: Duration::from_secs(5),
            js_domain: None,
        }
    }
}

impl ClientBuilder {
    /// Creates a new client builder
    pub fn new(nc: async_nats::Client) -> ClientBuilder {
        ClientBuilder {
            nc: Some(nc),
            ..Default::default()
        }
    }

    /// Sets the topic prefix for the NATS topic used for all control requests. Not to be confused with lattice ID/prefix
    pub fn topic_prefix(self, prefix: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            topic_prefix: Some(prefix.into()),
            ..self
        }
    }

    /// The lattice ID/prefix used for this client. If this function is not invoked, the prefix will be set to `default`
    pub fn lattice_prefix(self, prefix: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            ns_prefix: prefix.into(),
            ..self
        }
    }

    /// Sets the timeout for standard calls and RPC invocations used by the client. If not set, the default will be 2 seconds
    pub fn rpc_timeout(self, timeout: Duration) -> ClientBuilder {
        ClientBuilder {
            timeout: timeout,
            ..self
        }
    }

    /// Sets the timeout for auction (scatter/gather) operations. If not set, the default will be 5 seconds
    pub fn auction_timeout(self, timeout: Duration) -> ClientBuilder {
        ClientBuilder {
            auction_timeout: timeout,
            ..self
        }
    }

    /// Sets the JetStream domain for this client, which can be critical for locating the right key-value bucket
    /// for lattice metadata storage. If this is skipped, then the JS domain will be `None`
    pub fn js_domain(self, domain: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            js_domain: Some(domain.into()),
            ..self
        }
    }

    /// Completes the generation of a control interface client. This function is async because it will attempt
    /// to locate and attach to a metadata key-value bucket (`LATTICEDATA_{prefix}`) when starting. If this bucket
    /// is not discovered during build time, all subsequent client calls will operate in "legacy" mode against the
    /// deprecated control interface topics
    pub async fn build(self) -> Result<Client> {
        if let Some(nc) = self.nc {
            Ok(Client {
                nc: nc.clone(),
                topic_prefix: self.topic_prefix,
                ns_prefix: self.ns_prefix.clone(),
                timeout: self.timeout,
                auction_timeout: self.auction_timeout,
                kvstore: kv::get_kv_store(nc, &self.ns_prefix, self.js_domain).await,
            })
        } else {
            Err("Cannot create a control interface client without a NATS client".into())
        }
    }
}

impl Client {
    /// Creates a new lattice control interface client. You should use [ClientBuilder::new] instead. This
    /// function will also not attempt to communicate with a key-value store containing the lattice metadata
    /// and will only ever use the deprecated methods of host/lattice interaction
    #[deprecated(since = "0.23.0", note = "please use the client builder instead")]
    pub fn new(
        nc: async_nats::Client,
        ns_prefix: Option<String>,
        timeout: Duration,
        auction_timeout: Duration,
    ) -> Self {
        Client {
            nc,
            topic_prefix: None,
            ns_prefix: ns_prefix.unwrap_or_else(|| "default".to_string()),
            timeout,
            auction_timeout,
            kvstore: None,
        }
    }

    /// Creates a new lattice control interface client with a control interface topic
    /// prefix. You should use [ClientBuilder::new] instead.  This
    /// function will also not attempt to communicate with a key-value store containing the lattice metadata
    /// and will only ever use the deprecated methods of host/lattice interaction
    #[deprecated(since = "0.23.0", note = "please use the client builder instead")]
    pub fn new_with_topic_prefix(
        nc: async_nats::Client,
        topic_prefix: &str,
        ns_prefix: Option<String>,
        timeout: Duration,
        auction_timeout: Duration,
    ) -> Self {
        Client {
            nc,
            topic_prefix: Some(topic_prefix.to_owned()),
            ns_prefix: ns_prefix.unwrap_or_else(|| "default".to_string()),
            timeout,
            auction_timeout,
            kvstore: None,
        }
    }

    #[instrument(level = "debug", skip_all)]
    pub(crate) async fn request_timeout(
        &self,
        subject: String,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<async_nats::Message> {
        match tokio::time::timeout(
            timeout,
            self.nc.request_with_headers(
                subject,
                OtelHeaderInjector::default_with_span().into(),
                payload.into(),
            ),
        )
        .await
        {
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out").into()),
            Ok(Ok(message)) => Ok(message),
            Ok(Err(e)) => Err(e),
        }
    }

    /// Queries the lattice for all responsive hosts, waiting for the full period specified by _timeout_.
    #[instrument(level = "debug", skip_all)]
    pub async fn get_hosts(&self) -> Result<Vec<Host>> {
        let subject = broker::queries::hosts(&self.topic_prefix, &self.ns_prefix);
        debug!("get_hosts:publish {}", &subject);
        self.publish_and_wait(subject, Vec::new()).await
    }

    /// Retrieves the contents of a running host
    #[instrument(level = "debug", skip_all)]
    pub async fn get_host_inventory(&self, host_id: &str) -> Result<HostInventory> {
        let subject = broker::queries::host_inventory(&self.topic_prefix, &self.ns_prefix, host_id);
        debug!("get_host_inventory:request {}", &subject);
        match self.request_timeout(subject, vec![], self.timeout).await {
            Ok(msg) => {
                let hi: HostInventory = json_deserialize(&msg.payload)?;
                Ok(hi)
            }
            Err(e) => Err(format!("Did not receive host inventory from target host: {}", e).into()),
        }
    }

    /// Retrieves the full set of all cached claims in the lattice. If a suitable key-value bucket for metadata
    /// was discovered at client creation time, then that bucket will be queried directly for the claims. If not,
    /// then the claims will be queried by issuing a request on a queue-subscribed topic to the listening hosts.    
    #[instrument(level = "debug", skip_all)]
    pub async fn get_claims(&self) -> Result<GetClaimsResponse> {
        if let Some(ref store) = self.kvstore {
            kv::get_claims(store).await
        } else {
            let subject = broker::queries::claims(&self.topic_prefix, &self.ns_prefix);
            debug!("get_claims:request {}", &subject);
            match self.request_timeout(subject, vec![], self.timeout).await {
                Ok(msg) => {
                    let list: GetClaimsResponse = json_deserialize(&msg.payload)?;
                    Ok(list)
                }
                Err(e) => Err(format!("Did not receive claims from lattice: {}", e).into()),
            }
        }
    }

    /// Performs an actor auction within the lattice, publishing a set of constraints and the metadata for the actor
    /// in question. This will always wait for the full period specified by _duration_, and then return the set of
    /// gathered results. It is then up to the client to choose from among the "auction winners" to issue the appropriate
    /// command to start an actor. Clients cannot assume that auctions will always return at least one result.
    #[instrument(level = "debug", skip_all)]
    pub async fn perform_actor_auction(
        &self,
        actor_ref: &str,
        constraints: HashMap<String, String>,
    ) -> Result<Vec<ActorAuctionAck>> {
        let subject = broker::actor_auction_subject(&self.topic_prefix, &self.ns_prefix);
        let bytes = json_serialize(ActorAuctionRequest {
            actor_ref: actor_ref.to_string(),
            constraints,
        })?;
        debug!("actor_auction:publish {}", &subject);
        self.publish_and_wait(subject, bytes).await
    }

    /// Performs a provider auction within the lattice, publishing a set of constraints and the metadata for the provider
    /// in question. This will always wait for the full period specified by _duration_, and then return the set of gathered
    /// results. It is then up to the client to choose from among the "auction winners" and issue the appropriate command
    /// to start a provider. Clients cannot assume that auctions will always return at least one result.
    #[instrument(level = "debug", skip_all)]
    pub async fn perform_provider_auction(
        &self,
        provider_ref: &str,
        link_name: &str,
        constraints: HashMap<String, String>,
    ) -> Result<Vec<ProviderAuctionAck>> {
        let subject = broker::provider_auction_subject(&self.topic_prefix, &self.ns_prefix);
        let bytes = json_serialize(ProviderAuctionRequest {
            provider_ref: provider_ref.to_string(),
            link_name: link_name.to_string(),
            constraints,
        })?;
        debug!("provider_auction:publish {}", &subject);
        self.publish_and_wait(subject, bytes).await
    }

    /// Sends a request to the given host to start a given actor by its OCI reference. This returns an acknowledgement
    /// of _receipt_ of the command, not a confirmation that the actor started. An acknowledgement will either indicate
    /// some form of validation failure, or, if no failure occurs, the receipt of the command. To avoid blocking consumers,
    /// wasmCloud hosts will acknowledge the start actor command prior to fetching the actor's OCI bytes. If a client needs
    /// deterministic results as to whether the actor completed its startup process, the client will have to monitor
    /// the appropriate event in the control event stream
    #[instrument(level = "debug", skip_all)]
    pub async fn start_actor(
        &self,
        host_id: &str,
        actor_ref: &str,
        count: u16,
        annotations: Option<HashMap<String, String>>,
    ) -> Result<CtlOperationAck> {
        let subject = broker::commands::start_actor(&self.topic_prefix, &self.ns_prefix, host_id);
        debug!("start_actor:request {}", &subject);
        let bytes = json_serialize(StartActorCommand {
            count,
            actor_ref: actor_ref.to_string(),
            host_id: host_id.to_string(),
            annotations,
        })?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => {
                let ack: CtlOperationAck = json_deserialize(&msg.payload)?;
                Ok(ack)
            }
            Err(e) => Err(format!("Did not receive start actor acknowledgement: {}", e).into()),
        }
    }

    /// Sends a request to the given host to scale a given actor. This returns an acknowledgement of _receipt_ of the
    /// command, not a confirmation that the actor scaled. An acknowledgement will either indicate some form of
    /// validation failure, or, if no failure occurs, the receipt of the command. To avoid blocking consumers,
    /// wasmCloud hosts will acknowledge the scale actor command prior to fetching the actor's OCI bytes. If a client
    /// needs deterministic results as to whether the actor completed its startup process, the client will have to
    /// monitor the appropriate event in the control event stream
    #[instrument(level = "debug", skip_all)]
    pub async fn scale_actor(
        &self,
        host_id: &str,
        actor_ref: &str,
        actor_id: &str,
        count: u16,
        annotations: Option<HashMap<String, String>>,
    ) -> Result<CtlOperationAck> {
        let subject = broker::commands::scale_actor(&self.topic_prefix, &self.ns_prefix, host_id);
        debug!("scale_actor:request {}", &subject);
        let bytes = json_serialize(ScaleActorCommand {
            count,
            actor_ref: actor_ref.to_string(),
            host_id: host_id.to_string(),
            actor_id: actor_id.to_string(),
            annotations,
        })?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => {
                let ack: CtlOperationAck = json_deserialize(&msg.payload)?;
                Ok(ack)
            }
            Err(e) => Err(format!("Did not receive scale actor acknowledgement: {}", e).into()),
        }
    }

    /// Publishes a registry credential map to the control interface of the lattice.
    /// All hosts will be listening and all will overwrite their registry credential
    /// map with the new information. It is highly recommended you use TLS connections
    /// with NATS and isolate the control interface credentials when using this
    /// function in production as the data contains secrets
    #[instrument(level = "debug", skip_all)]
    pub async fn put_registries(&self, registries: RegistryCredentialMap) -> Result<()> {
        let subject = broker::publish_registries(&self.topic_prefix, &self.ns_prefix);
        debug!("put_registries:publish {}", &subject);
        let bytes = json_serialize(&registries)?;
        let resp = self
            .nc
            .publish_with_headers(
                subject,
                OtelHeaderInjector::default_with_span().into(),
                bytes.into(),
            )
            .await;
        if let Err(e) = resp {
            Err(format!("Failed to push registry credential map: {}", e).into())
        } else {
            Ok(())
        }
    }

    /// If a key-value bucket was discovered at client construction time, then the link data will be written directly
    /// to the bucket and interested parties will be notified indirectly by virtue of key subscription/monitoring. If
    /// no bucket was discovered, then the "old" behavior will be performed of publishing the link data on the
    /// appropriate topic.    
    #[instrument(level = "debug", skip_all)]
    pub async fn advertise_link(
        &self,
        actor_id: &str,
        provider_id: &str,
        contract_id: &str,
        link_name: &str,
        values: HashMap<String, String>,
    ) -> Result<CtlOperationAck> {
        let mut ld = LinkDefinition::default();
        ld.actor_id = actor_id.to_string();
        ld.provider_id = provider_id.to_string();
        ld.contract_id = contract_id.to_string();
        ld.link_name = link_name.to_string();
        ld.values = values;

        if let Some(ref store) = self.kvstore {
            kv::put_link(store, ld).await.map(|_| CtlOperationAck {
                accepted: true,
                error: "".to_string(),
            })
        } else {
            let subject = broker::advertise_link(&self.topic_prefix, &self.ns_prefix);
            debug!("advertise_link:request {}", &subject);

            let bytes = crate::json_serialize(&ld)?;
            match self.request_timeout(subject, bytes, self.timeout).await {
                Ok(msg) => {
                    let ack: CtlOperationAck = json_deserialize(&msg.payload)?;
                    Ok(ack)
                }
                Err(e) => {
                    Err(format!("Did not receive advertise link acknowledgement: {}", e).into())
                }
            }
        }
    }

    /// If a key-value bucket is being used, then the link definition will be removed from that bucket directly. If not,
    /// then this function will fall back to publishing a link definition removal request on the right lattice control
    /// interface topic.
    #[instrument(level = "debug", skip_all)]
    pub async fn remove_link(
        &self,
        actor_id: &str,
        contract_id: &str,
        link_name: &str,
    ) -> Result<CtlOperationAck> {
        if let Some(ref store) = self.kvstore {
            match kv::delete_link(store, actor_id, contract_id, link_name).await {
                Ok(_) => Ok(CtlOperationAck {
                    accepted: true,
                    error: "".to_string(),
                }),
                Err(e) => Ok(CtlOperationAck {
                    accepted: false,
                    error: format!("{}", e),
                }),
            }
        } else {
            let subject = broker::remove_link(&self.topic_prefix, &self.ns_prefix);
            debug!("remove_link:request {}", &subject);
            let mut ld = LinkDefinition::default();
            ld.actor_id = actor_id.to_string();
            ld.contract_id = contract_id.to_string();
            ld.link_name = link_name.to_string();
            let bytes = crate::json_serialize(&ld)?;
            match self.request_timeout(subject, bytes, self.timeout).await {
                Ok(msg) => {
                    let ack: CtlOperationAck = json_deserialize(&msg.payload)?;
                    Ok(ack)
                }
                Err(e) => Err(format!("Did not receive remove link acknowledgement: {}", e).into()),
            }
        }
    }

    /// Retrieves the list of link definitions stored in the lattice metadata key-value bucket. If no such bucket was discovered
    /// at client creation time, then it will issue a "legacy" request on the appropriate topic to request link definitions
    /// from the first host that answers that request.    
    #[instrument(level = "debug", skip_all)]
    pub async fn query_links(&self) -> Result<LinkDefinitionList> {
        if let Some(ref store) = self.kvstore {
            kv::get_links(store).await
        } else {
            let subject = broker::queries::link_definitions(&self.topic_prefix, &self.ns_prefix);
            debug!("query_links:request {}", &subject);
            match self.request_timeout(subject, vec![], self.timeout).await {
                Ok(msg) => json_deserialize(&msg.payload),
                Err(e) => Err(format!("Did not receive a response to links query: {}", e).into()),
            }
        }
    }

    /// Issue a command to a host instructing that it replace an existing actor (indicated by its
    /// public key) with a new actor indicated by an OCI image reference. The host will acknowledge
    /// this request as soon as it verifies that the target actor is running. This acknowledgement
    /// occurs **before** the new bytes are downloaded. Live-updating an actor can take a long
    /// time and control clients cannot block waiting for a reply that could come several seconds
    /// later. If you need to verify that the actor has been updated, you will want to set up a
    /// listener for the appropriate **PublishedEvent** which will be published on the control events
    /// channel in JSON
    #[instrument(level = "debug", skip_all)]
    pub async fn update_actor(
        &self,
        host_id: &str,
        existing_actor_id: &str,
        new_actor_ref: &str,
        annotations: Option<HashMap<String, String>>,
    ) -> Result<CtlOperationAck> {
        let subject = broker::commands::update_actor(&self.topic_prefix, &self.ns_prefix, host_id);
        debug!("update_actor:request {}", &subject);
        let bytes = json_serialize(UpdateActorCommand {
            host_id: host_id.to_string(),
            actor_id: existing_actor_id.to_string(),
            new_actor_ref: new_actor_ref.to_string(),
            annotations,
        })?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => {
                let ack: CtlOperationAck = json_deserialize(&msg.payload)?;
                Ok(ack)
            }
            Err(e) => Err(format!("Did not receive update actor acknowledgement: {}", e).into()),
        }
    }

    /// Issues a command to a host to start a provider with a given OCI reference using the specified link
    /// name (or "default" if none is specified). The target wasmCloud host will acknowledge the receipt
    /// of this command _before_ downloading the provider's bytes from the OCI registry, indicating either
    /// a validation failure or success. If a client needs deterministic guarantees that the provider has
    /// completed its startup process, such a client needs to monitor the control event stream for the
    /// appropriate event. If a host ID is not supplied (empty string), then this function will return
    /// an early acknowledgement, go find a host, and then submit the start request to a target host.
    #[instrument(level = "debug", skip_all)]
    pub async fn start_provider(
        &self,
        host_id: &str,
        provider_ref: &str,
        link_name: Option<String>,
        annotations: Option<HashMap<String, String>>,
        provider_configuration: Option<String>,
    ) -> Result<CtlOperationAck> {
        let provider_ref = provider_ref.to_string();
        if !host_id.trim().is_empty() {
            start_provider_(
                &self.nc,
                &self.topic_prefix,
                &self.ns_prefix,
                self.timeout,
                host_id,
                &provider_ref,
                link_name,
                annotations,
                provider_configuration,
            )
            .in_current_span()
            .await
        } else {
            // If a host isn't supplied, try to find one via auction.
            // If no host is found, return error.
            // If a host is found, start brackground request to start provider and return Ack
            let mut error = String::new();
            debug!("start_provider:deferred (no-host) request");
            let current_span = tracing::Span::current();
            let host = match self.get_hosts().await {
                Err(e) => {
                    error = format!("failed to query hosts for no-host provider start: {}", e);
                    None
                }
                Ok(hs) => hs.into_iter().next(),
            };
            if let Some(host) = host {
                let this = self.clone();
                tokio::spawn(async move {
                    let _ = start_provider_(
                        &this.nc,
                        &this.topic_prefix,
                        &this.ns_prefix,
                        this.timeout,
                        &host.id,
                        &provider_ref,
                        link_name,
                        annotations,
                        provider_configuration,
                    )
                    .instrument(current_span)
                    .await;
                });
            } else if error.is_empty() {
                error = "No hosts detected in in no-host provider start.".to_string();
            }
            if !error.is_empty() {
                error!("{}", error);
            }
            Ok(CtlOperationAck {
                accepted: true,
                error,
            })
        }
    }

    /// Issues a command to a host to stop a provider for the given OCI reference, link name, and contract ID. The
    /// target wasmCloud host will acknowledge the receipt of this command, and _will not_ supply a discrete
    /// confirmation that a provider has terminated. For that kind of information, the client must also monitor
    /// the control event stream
    #[instrument(level = "debug", skip_all)]
    pub async fn stop_provider(
        &self,
        host_id: &str,
        provider_ref: &str,
        link_name: &str,
        contract_id: &str,
        annotations: Option<HashMap<String, String>>,
    ) -> Result<CtlOperationAck> {
        let subject = broker::commands::stop_provider(&self.topic_prefix, &self.ns_prefix, host_id);
        debug!("stop_provider:request {}", &subject);
        let bytes = json_serialize(StopProviderCommand {
            host_id: host_id.to_string(),
            provider_ref: provider_ref.to_string(),
            link_name: link_name.to_string(),
            contract_id: contract_id.to_string(),
            annotations,
        })?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => {
                let ack: CtlOperationAck = json_deserialize(&msg.payload)?;
                Ok(ack)
            }
            Err(e) => Err(format!("Did not receive stop provider acknowledgement: {}", e).into()),
        }
    }

    /// Issues a command to a host to stop an actor for the given OCI reference. The
    /// target wasmCloud host will acknowledge the receipt of this command, and _will not_ supply a discrete
    /// confirmation that the actor has terminated. For that kind of information, the client must also monitor
    /// the control event stream
    #[instrument(level = "debug", skip_all)]
    pub async fn stop_actor(
        &self,
        host_id: &str,
        actor_ref: &str,
        count: u16,
        annotations: Option<HashMap<String, String>>,
    ) -> Result<CtlOperationAck> {
        let subject = broker::commands::stop_actor(&self.topic_prefix, &self.ns_prefix, host_id);
        debug!("stop_actor:request {}", &subject);
        let bytes = json_serialize(StopActorCommand {
            host_id: host_id.to_string(),
            actor_ref: actor_ref.to_string(),
            count,
            annotations,
        })?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => {
                let ack: CtlOperationAck = json_deserialize(&msg.payload)?;
                Ok(ack)
            }
            Err(e) => Err(format!("Did not receive stop actor acknowledgement: {}", e).into()),
        }
    }

    /// Issues a command to a specific host to perform a graceful termination. The target host
    /// will acknowledge receipt of the command before it attempts a shutdown. To deterministically
    /// verify that the host is down, a client should monitor for the "host stopped" event or
    /// passively detect the host down by way of a lack of heartbeat receipts
    #[instrument(level = "debug", skip_all)]
    pub async fn stop_host(
        &self,
        host_id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<CtlOperationAck> {
        let subject = broker::commands::stop_host(&self.topic_prefix, &self.ns_prefix, host_id);
        debug!("stop_host:request {}", &subject);
        let bytes = json_serialize(StopHostCommand {
            host_id: host_id.to_owned(),
            timeout: timeout_ms,
        })?;

        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => {
                let ack: CtlOperationAck = json_deserialize(&msg.payload)?;
                Ok(ack)
            }
            Err(e) => Err(format!("Did not receive stop host acknowledgement: {}", e).into()),
        }
    }

    async fn publish_and_wait<T: DeserializeOwned>(
        &self,
        subject: String,
        payload: Vec<u8>,
    ) -> Result<Vec<T>> {
        let reply = self.nc.new_inbox();
        let sub = self.nc.subscribe(reply.clone()).await?;
        self.nc
            .publish_with_reply_and_headers(
                subject.clone(),
                reply,
                OtelHeaderInjector::default_with_span().into(),
                payload.into(),
            )
            .await?;
        let nc = self.nc.clone();
        tokio::spawn(async move {
            if let Err(error) = nc.flush().await {
                error!(%error, "flush after publish");
            }
        });
        Ok(collect_timeout::<T>(sub, self.auction_timeout, subject.as_str()).await)
    }

    /// Returns the receiver end of a channel that subscribes to the lattice control event stream.
    /// Any [`Event`](struct@Event)s that are published after this channel is created
    /// will be added to the receiver channel's buffer, which can be observed or handled if needed.
    /// See the example for how you could use this receiver to handle events.
    ///
    /// # Example
    /// ```rust
    /// use wasmcloud_control_interface::{Client, ClientBuilder};
    /// async {
    ///   let nc = async_nats::connect("127.0.0.1:4222").await.unwrap();
    ///   let client = ClientBuilder::new(nc)
    ///                 .rpc_timeout(std::time::Duration::from_millis(1000))
    ///                 .auction_timeout(std::time::Duration::from_millis(1000))
    ///                 .build().await.unwrap();
    ///   let mut receiver = client.events_receiver().await.unwrap();
    ///   tokio::spawn( async move {
    ///       while let Some(evt) = receiver.recv().await {
    ///           println!("Event received: {:?}", evt);
    ///       }
    ///   });
    ///   // perform other operations on client
    ///   client.get_host_inventory("NAEXHW...").await.unwrap();
    /// };
    /// ```
    ///
    /// Once you're finished with the event receiver, be sure to call `drop` with the receiver
    /// as an argument. This closes the channel and will prevent the sender from endlessly
    /// sending messages into the channel buffer.
    ///
    /// # Example
    /// ```rust
    /// use wasmcloud_control_interface::{Client, ClientBuilder};
    /// async {
    ///   let nc = async_nats::connect("0.0.0.0:4222").await.unwrap();
    ///   let client = ClientBuilder::new(nc)
    ///                 .rpc_timeout(std::time::Duration::from_millis(1000))
    ///                 .auction_timeout(std::time::Duration::from_millis(1000))
    ///                 .build().await.unwrap();    
    ///   let mut receiver = client.events_receiver().await.unwrap();
    ///   // read the docs for flume receiver. You can use it in either sync or async code
    ///   // The receiver can be cloned() as needed.
    ///   // If you drop the receiver. The subscriber will exit
    ///   // If the nats connection ic closed, the loop below will exit.
    ///   while let Some(evt) = receiver.recv().await {
    ///       println!("Event received: {:?}", evt);
    ///   }
    /// };
    /// ```
    pub async fn events_receiver(&self) -> Result<Receiver<Event>> {
        use futures::StreamExt as _;
        let (sender, receiver) = tokio::sync::mpsc::channel(5000);
        let mut sub = self
            .nc
            .subscribe(broker::control_event(&self.ns_prefix))
            .await?;
        tokio::spawn(async move {
            while let Some(msg) = sub.next().await {
                let evt = match json_deserialize::<Event>(&msg.payload) {
                    Ok(evt) => evt,
                    Err(_) => {
                        error!("Object received on event stream was not a CloudEvent");
                        continue;
                    }
                };
                trace!("received event: {:?}", evt);
                // If the channel is disconnected, stop sending events
                if sender.send(evt).await.is_err() {
                    let _ = sub.unsubscribe().await;
                    break;
                }
            }
        });
        Ok(receiver)
    }
}

// [ss]: renamed to json_serialize and json_deserialize to avoid confusion
//   with msgpack serialize and deserialize, used for rpc messages.
//
/// The standard function for serializing codec structs into a format that can be
/// used for message exchange between actor and host. Use of any other function to
/// serialize could result in breaking incompatibilities.
pub fn json_serialize<T>(
    item: T,
) -> ::std::result::Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>
where
    T: Serialize,
{
    serde_json::to_vec(&item).map_err(|e| format!("JSON serialization failure: {}", e).into())
}

/// The standard function for de-serializing codec structs from a format suitable
/// for message exchange between actor and host. Use of any other function to
/// deserialize could result in breaking incompatibilities.
pub fn json_deserialize<'de, T: Deserialize<'de>>(
    buf: &'de [u8],
) -> ::std::result::Result<T, Box<dyn std::error::Error + Send + Sync>> {
    serde_json::from_slice(buf).map_err(|e| {
        {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("JSON deserialization failure: {}", e),
            )
        }
        .into()
    })
}

// "selfless" helper function that submits a start provider request to a host
#[allow(clippy::too_many_arguments)]
async fn start_provider_(
    client: &async_nats::Client,
    topic_prefix: &Option<String>,
    ns_prefix: &str,
    timeout: Duration,
    host_id: &str,
    provider_ref: &str,
    link_name: Option<String>,
    annotations: Option<HashMap<String, String>>,
    provider_configuration: Option<String>,
) -> Result<CtlOperationAck> {
    let subject = broker::commands::start_provider(topic_prefix, ns_prefix, host_id);
    debug!("start_provider:request {}", &subject);
    let bytes = json_serialize(StartProviderCommand {
        host_id: host_id.to_string(),
        provider_ref: provider_ref.to_string(),
        link_name: link_name.unwrap_or_else(|| "default".to_string()),
        annotations,
        configuration: provider_configuration,
    })?;
    match tokio::time::timeout(
        timeout,
        client.request_with_headers(
            subject,
            OtelHeaderInjector::default_with_span().into(),
            bytes.into(),
        ),
    )
    .await
    {
        Err(e) => Err(format!("Did not receive start provider acknowledgement: {}", e).into()),
        Ok(Err(e)) => Err(format!("Error sending or receiving message: {}", e).into()),
        Ok(Ok(msg)) => {
            let ack: CtlOperationAck = json_deserialize(&msg.payload)?;
            Ok(ack)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Note: This test is a means of manually watching the event stream as CloudEvents are received
    /// It does not assert functionality, and so we've marked it as ignore to ensure it's not run by default
    /// It currently listens for 120 seconds then exits
    #[tokio::test]
    #[ignore]
    async fn test_events_receiver() {
        let nc = async_nats::connect("127.0.0.1:4222").await.unwrap();
        let client = ClientBuilder::new(nc)
            .rpc_timeout(Duration::from_millis(1000))
            .auction_timeout(Duration::from_millis(1000))
            .build()
            .await
            .unwrap();
        let mut receiver = client.events_receiver().await.unwrap();
        tokio::spawn(async move {
            while let Some(evt) = receiver.recv().await {
                println!("Event received: {:?}", evt);
            }
        });
        println!("Listening to Cloud Events for 120 seconds. Then we will quit.");
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
    }
}
