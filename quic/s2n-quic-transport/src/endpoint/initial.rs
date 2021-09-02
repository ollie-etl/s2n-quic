// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    connection::{
        self,
        id::{ConnectionInfo, Generator as _},
        limits::{ConnectionInfo as LimitsInfo, Limiter as _},
        Trait as _,
    },
    endpoint,
    recovery::congestion_controller::{self, Endpoint as _},
    space::PacketSpaceManager,
};
use core::convert::TryInto;
use s2n_codec::DecoderBufferMut;
use s2n_quic_core::{
    crypto::{tls, tls::Endpoint as TLSEndpoint, CryptoSuite, InitialKey},
    event::{self, IntoEvent, Subscriber as _},
    inet::{datagram, DatagramInfo},
    packet::initial::ProtectedInitial,
    path::Handle as _,
    stateless_reset::token::Generator as _,
    transport::{self, parameters::ServerTransportParameters},
};

impl<Config: endpoint::Config> endpoint::Endpoint<Config> {
    pub(super) fn handle_initial_packet(
        &mut self,
        header: &datagram::Header<Config::PathHandle>,
        datagram: &DatagramInfo,
        packet: ProtectedInitial,
        remaining: DecoderBufferMut,
        retry_token_dcid: Option<connection::InitialId>,
    ) -> Result<(), connection::Error> {
        debug_assert!(
            Config::ENDPOINT_TYPE.is_server(),
            "only servers can accept new initial connections"
        );

        //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#14.1
        //# A client MUST expand the payload of all UDP datagrams carrying
        //# Initial packets to at least the smallest allowed maximum datagram
        //# size of 1200 bytes

        //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#14.1
        //# A server MUST discard an Initial packet that is carried in a UDP
        //# datagram with a payload that is smaller than the smallest allowed
        //# maximum datagram size of 1200 bytes.

        //= https://tools.ietf.org/id/draft-ietf-quic-tls-32.txt#9.3
        //# First, the packet
        //# containing a ClientHello MUST be padded to a minimum size.
        if datagram.payload_len < 1200 {
            return Err(transport::Error::PROTOCOL_VIOLATION
                .with_reason("packet too small")
                .into());
        }

        let remote_address = header.path.remote_address();

        // The first connection ID to persist and use for routing incoming packets
        let initial_connection_id;
        // The randomly generated destination connection ID that was sent from the client
        let original_destination_connection_id;

        if let Some(retry_dcid) = retry_token_dcid {
            original_destination_connection_id = retry_dcid;
            // This initial packet was in response to a Retry, so the destination connection ID
            // on the packet was generated by this server. We can use this destination connection
            // ID as the initial_connection_id.
            initial_connection_id = datagram.destination_connection_id;
        } else {
            //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#7.2
            //# When an Initial packet is sent by a client that has not previously
            //# received an Initial or Retry packet from the server, the client
            //# populates the Destination Connection ID field with an unpredictable
            //# value.  This Destination Connection ID MUST be at least 8 bytes in
            //# length.
            original_destination_connection_id =
                datagram.destination_connection_id.try_into().map_err(|_| {
                    transport::Error::PROTOCOL_VIOLATION
                        .with_reason("destination connection id too short")
                })?;
            // The destination connection ID on the packet was randomly generated by the client
            // so we'll generate a new initial_connection_id.
            let connection_info = ConnectionInfo::new(&remote_address);
            initial_connection_id = self
                .config
                .context()
                .connection_id_format
                .generate(&connection_info);
        }

        //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#17.2
        //# Endpoints that receive a version 1 long header
        //# with a value larger than 20 MUST drop the packet.
        let source_connection_id: connection::PeerId = packet
            .source_connection_id()
            .try_into()
            .map_err(transport::Error::from)?;

        //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#17.2.5.2
        //# Changing Destination Connection ID also results in a change
        //# to the keys used to protect the Initial packet.
        let (initial_key, initial_header_key) =
            <<Config::TLSEndpoint as tls::Endpoint>::Session as CryptoSuite>::InitialKey::new_server(
                datagram.destination_connection_id.as_bytes(),
            );

        let largest_packet_number = Default::default();
        let packet = packet.unprotect(&initial_header_key, largest_packet_number)?;
        let packet = packet.decrypt(&initial_key)?;

        // TODO handle token with stateless retry

        let internal_connection_id = self.connection_id_generator.generate_id();

        let stateless_reset_token = self
            .config
            .context()
            .stateless_reset_token_generator
            .generate(initial_connection_id.as_bytes());

        let local_id_registry = self.connection_id_mapper.create_local_id_registry(
            internal_connection_id,
            &initial_connection_id,
            stateless_reset_token,
        );

        //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#10.3
        //= type=TODO
        //= tracking-issue=195
        //= feature=Stateless Reset
        //# Servers can also specify a stateless_reset_token transport
        //# parameter during the handshake that applies to the connection ID that
        //# it selected during the handshake; clients cannot use this transport
        //# parameter because their transport parameters do not have
        //# confidentiality protection.
        let stateless_reset_token = None;
        let peer_id_registry = self.connection_id_mapper.create_peer_id_registry(
            internal_connection_id,
            source_connection_id,
            stateless_reset_token,
        );

        let wakeup_handle = self
            .wakeup_queue
            .create_wakeup_handle(internal_connection_id);

        let mut transport_parameters = ServerTransportParameters::default();

        let limits = self
            .config
            .context()
            .connection_limits
            .on_connection(&LimitsInfo::new(&remote_address));

        transport_parameters.load_limits(&limits);

        //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#7.3
        //# A server includes the Destination Connection ID field from the first
        //# Initial packet it received from the client in the
        //# original_destination_connection_id transport parameter; if the server
        //# sent a Retry packet, this refers to the first Initial packet received
        //# before sending the Retry packet.  If it sends a Retry packet, a
        //# server also includes the Source Connection ID field from the Retry
        //# packet in the retry_source_connection_id transport parameter.
        transport_parameters.original_destination_connection_id = Some(
            original_destination_connection_id
                .try_into()
                .expect("connection ID already validated"),
        );
        if retry_token_dcid.is_some() {
            transport_parameters.retry_source_connection_id = Some(
                datagram
                    .destination_connection_id
                    .try_into()
                    .expect("failed to convert source connection id"),
            );
        }

        //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#7.3
        //# Each endpoint includes the value of the Source Connection ID field
        //# from the first Initial packet it sent in the
        //# initial_source_connection_id transport parameter
        transport_parameters.initial_source_connection_id = Some(
            initial_connection_id
                .as_bytes()
                .try_into()
                .expect("connection ID already validated"),
        );

        //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#18.2
        //# The active connection ID limit is an integer value specifying the
        //# maximum number of connection IDs from the peer that an endpoint is
        //# willing to store. This value includes the connection ID received
        //# during the handshake, that received in the preferred_address transport
        //# parameter, and those received in NEW_CONNECTION_ID frames.
        transport_parameters.active_connection_id_limit = s2n_quic_core::varint::VarInt::from(
            connection::peer_id_registry::ACTIVE_CONNECTION_ID_LIMIT,
        )
        .try_into()
        .unwrap();

        let endpoint_context = self.config.context();

        let tls_session = endpoint_context
            .tls
            .new_server_session(&transport_parameters);

        let path_info = congestion_controller::PathInfo::new(&remote_address);
        let congestion_controller = endpoint_context
            .congestion_controller
            .new_congestion_controller(path_info);

        let quic_version = packet.version;

        let meta = event::builder::ConnectionMeta {
            endpoint_type: Config::ENDPOINT_TYPE,
            id: internal_connection_id.into(),
            timestamp: datagram.timestamp,
        };
        let mut event_context = endpoint_context
            .event_subscriber
            .create_connection_context(&meta.clone().into_event());

        let mut publisher = event::ConnectionPublisherSubscriber::new(
            meta,
            quic_version,
            endpoint_context.event_subscriber,
            &mut event_context,
        );

        let space_manager = PacketSpaceManager::new(
            tls_session,
            initial_key,
            initial_header_key,
            datagram.timestamp,
            &mut publisher,
        );

        let connection_parameters = connection::Parameters {
            internal_connection_id,
            local_id_registry,
            peer_id_registry,
            space_manager,
            wakeup_handle,
            //= https://tools.ietf.org/id/draft-ietf-quic-transport-32.txt#7.2
            //# A server MUST set the Destination Connection ID it
            //# uses for sending packets based on the first received Initial packet.
            peer_connection_id: source_connection_id,
            local_connection_id: initial_connection_id,
            path_handle: header.path,
            congestion_controller,
            timestamp: datagram.timestamp,
            quic_version,
            limits,
            max_mtu: self.max_mtu,
            event_context,
            event_subscriber: endpoint_context.event_subscriber,
        };

        let mut connection = <Config as endpoint::Config>::Connection::new(connection_parameters);

        let path_id = connection.on_datagram_received(
            &header.path,
            datagram,
            endpoint_context.congestion_controller,
            endpoint_context.random_generator,
            self.max_mtu,
            endpoint_context.event_subscriber,
        )?;

        connection
            .handle_cleartext_initial_packet(
                datagram,
                path_id,
                packet,
                endpoint_context.random_generator,
                endpoint_context.event_subscriber,
            )
            .map_err(|err| {
                use connection::ProcessingError;
                match err {
                    ProcessingError::CryptoError(err) => err.into(),
                    ProcessingError::ConnectionError(err) => err,
                    // this is the first packet this connection has received
                    // so getting this error would be incorrect
                    ProcessingError::DuplicatePacket => {
                        if cfg!(debug_assertions) {
                            panic!("got duplicate packet error on first packet");
                        }
                        transport::Error::INTERNAL_ERROR.into()
                    }
                }
            })?;

        connection.handle_remaining_packets(
            &header.path,
            datagram,
            path_id,
            endpoint_context.connection_id_format,
            remaining,
            endpoint_context.random_generator,
            endpoint_context.event_subscriber,
        )?;

        //= https://tools.ietf.org/id/draft-ietf-quic-tls-32.txt#4.3
        //= type=TODO
        //= tracking-issue=299
        //# If the
        //# ClientHello spans multiple Initial packets, such servers would need
        //# to buffer the first received fragments, which could consume excessive
        //# resources if the client's address has not yet been validated.  To
        //# avoid this, servers MAY use the Retry feature (see Section 8.1 of
        //# [QUIC-TRANSPORT]) to only buffer partial ClientHello messages from
        //# clients with a validated address.

        let result = self
            .connection_id_mapper
            .try_insert_initial_id(original_destination_connection_id, internal_connection_id);

        debug_assert!(
            result.is_ok(),
            "Initial ID {:?} was already in the map",
            original_destination_connection_id
        );

        // Only persist the connection if everything is good.
        // Otherwise the connection will automatically get dropped. This
        // will also clean up all state which was already allocated for
        // the connection
        self.connections
            .insert_connection(connection, internal_connection_id);

        Ok(())
    }
}
