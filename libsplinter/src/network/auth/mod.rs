// Copyright 2018-2020 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod handlers;

use std::collections::HashMap;
use std::fmt;
use std::sync::{
    mpsc::{channel, Receiver},
    Arc, Mutex,
};

use crate::network::Network;

/// The states of a connection during authorization.
#[derive(PartialEq, Debug, Clone)]
enum AuthorizationState {
    Unknown,
    Connecting,
    Authorized,
    Unauthorized,
    Internal,
}

impl fmt::Display for AuthorizationState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            AuthorizationState::Unknown => "Unknown",
            AuthorizationState::Connecting => "Connecting",
            AuthorizationState::Authorized => "Authorized",
            AuthorizationState::Unauthorized => "Unauthorized",
            AuthorizationState::Internal => "Internal",
        })
    }
}

type Identity = String;

/// The state transitions that can be applied on an connection during authorization.
#[derive(PartialEq, Debug)]
enum AuthorizationAction {
    Connecting,
    TrustIdentifying(Identity),
    Unauthorizing,
}

impl fmt::Display for AuthorizationAction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            AuthorizationAction::Connecting => "Connecting",
            AuthorizationAction::TrustIdentifying(_) => "TrustIdentifying",
            AuthorizationAction::Unauthorizing => "Unauthorizing",
        })
    }
}

/// The errors that may occur for a connection during authorization.
#[derive(PartialEq, Debug)]
enum AuthorizationActionError {
    AlreadyConnecting,
    InvalidMessageOrder(AuthorizationState, AuthorizationAction),
    ConnectionLost,
}

impl fmt::Display for AuthorizationActionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AuthorizationActionError::AlreadyConnecting => {
                f.write_str("Already attempting to connect.")
            }
            AuthorizationActionError::InvalidMessageOrder(start, action) => {
                write!(f, "Attempting to transition from {} via {}.", start, action)
            }
            AuthorizationActionError::ConnectionLost => {
                f.write_str("Connection lost while authorizing peer")
            }
        }
    }
}

#[derive(Debug)]
pub struct AuthorizationCallbackError(pub String);

impl std::error::Error for AuthorizationCallbackError {}

impl fmt::Display for AuthorizationCallbackError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "unable to register callback: {}", self.0)
    }
}

pub trait AuthorizationInquisitor: Send {
    /// Register a callback to receive notifications about peer authorization statuses.
    fn register_callback(
        &self,
        callback: Box<dyn AuthorizationCallback>,
    ) -> Result<(), AuthorizationCallbackError>;

    /// Indicates whether or not a peer is authorized.
    fn is_authorized(&self, peer_id: &str) -> bool;
}

/// Manages authorization states for connections on a network.
#[derive(Clone)]
pub struct AuthorizationManager {
    shared: Arc<Mutex<ManagedAuthorizations>>,
    network: Network,
    identity: Identity,
}

impl AuthorizationManager {
    /// Constructs an AuthorizationManager
    pub fn new(network: Network, identity: Identity) -> Self {
        let (disconnect_send, disconnect_receive) = channel();
        let shared = Arc::new(Mutex::new(ManagedAuthorizations::new(disconnect_receive)));

        network.add_disconnect_listener(Box::new(move |peer_id: &str| {
            match disconnect_send.send(peer_id.to_string()) {
                Ok(()) => (),
                Err(_) => error!("unable to notify authorization manager of disconnection"),
            }
        }));

        AuthorizationManager {
            shared,
            network,
            identity,
        }
    }

    /// Transitions from one authorization state to another
    ///
    /// Errors
    ///
    /// The errors are error messages that should be returned on the appropriate message
    fn next_state(
        &self,
        peer_id: &str,
        action: AuthorizationAction,
    ) -> Result<AuthorizationState, AuthorizationActionError> {
        let mut shared = mutex_lock_unwrap!(self.shared);

        // drain the removals
        let removals = shared.disconnect_receiver.try_iter().collect::<Vec<_>>();
        for peer_id in removals.into_iter() {
            shared.states.remove(&peer_id);
        }

        let cur_state = shared
            .states
            .get(peer_id)
            .unwrap_or(&AuthorizationState::Unknown);
        match *cur_state {
            AuthorizationState::Unknown => match action {
                AuthorizationAction::Connecting => {
                    if let Some(endpoint) = self.network.get_peer_endpoint(peer_id) {
                        if endpoint.contains("inproc") {
                            // Automatically authorize inproc connections
                            debug!("Authorize inproc connection: {}", peer_id);
                            shared
                                .states
                                .insert(peer_id.to_string(), AuthorizationState::Internal);
                            Self::notify_callbacks(
                                &shared.callbacks,
                                peer_id,
                                PeerAuthorizationState::Authorized,
                            );
                            return Ok(AuthorizationState::Internal);
                        }
                    }
                    // Here the decision for Challenges will be made.
                    shared
                        .states
                        .insert(peer_id.to_string(), AuthorizationState::Connecting);
                    Ok(AuthorizationState::Connecting)
                }
                AuthorizationAction::Unauthorizing => {
                    self.network
                        .remove_connection(&peer_id.to_string())
                        .map_err(|_| AuthorizationActionError::ConnectionLost)?;
                    Ok(AuthorizationState::Unauthorized)
                }
                _ => Err(AuthorizationActionError::InvalidMessageOrder(
                    AuthorizationState::Unknown,
                    action,
                )),
            },
            AuthorizationState::Connecting => match action {
                AuthorizationAction::Connecting => Err(AuthorizationActionError::AlreadyConnecting),
                AuthorizationAction::TrustIdentifying(new_peer_id) => {
                    // Verify pub key allowed
                    shared.states.remove(peer_id);
                    self.network
                        .update_peer_id(peer_id.to_string(), new_peer_id.clone())
                        .map_err(|_| AuthorizationActionError::ConnectionLost)?;
                    shared
                        .states
                        .insert(new_peer_id.clone(), AuthorizationState::Authorized);
                    Self::notify_callbacks(
                        &shared.callbacks,
                        &new_peer_id,
                        PeerAuthorizationState::Authorized,
                    );
                    Ok(AuthorizationState::Authorized)
                }
                AuthorizationAction::Unauthorizing => {
                    shared.states.remove(peer_id);
                    self.network
                        .remove_connection(&peer_id.to_string())
                        .map_err(|_| AuthorizationActionError::ConnectionLost)?;
                    Self::notify_callbacks(
                        &shared.callbacks,
                        peer_id,
                        PeerAuthorizationState::Unauthorized,
                    );
                    Ok(AuthorizationState::Unauthorized)
                }
            },
            AuthorizationState::Authorized => match action {
                AuthorizationAction::Unauthorizing => {
                    shared.states.remove(peer_id);
                    self.network
                        .remove_connection(&peer_id.to_string())
                        .map_err(|_| AuthorizationActionError::ConnectionLost)?;
                    Self::notify_callbacks(
                        &shared.callbacks,
                        peer_id,
                        PeerAuthorizationState::Unauthorized,
                    );
                    Ok(AuthorizationState::Unauthorized)
                }
                _ => Err(AuthorizationActionError::InvalidMessageOrder(
                    AuthorizationState::Authorized,
                    action,
                )),
            },
            _ => Err(AuthorizationActionError::InvalidMessageOrder(
                cur_state.clone(),
                action,
            )),
        }
    }

    fn notify_callbacks(
        callbacks: &[Box<dyn AuthorizationCallback>],
        peer_id: &str,
        state: PeerAuthorizationState,
    ) {
        for callback in callbacks {
            if let Err(err) = callback.on_authorization_change(peer_id, state.clone()) {
                error!("Unable to call authorization change callback: {}", err);
            }
        }
    }
}

impl AuthorizationInquisitor for AuthorizationManager {
    fn register_callback(
        &self,
        callback: Box<dyn AuthorizationCallback>,
    ) -> Result<(), AuthorizationCallbackError> {
        let mut shared = self
            .shared
            .lock()
            .map_err(|_| AuthorizationCallbackError("shared state lock was poisoned".into()))?;

        shared.callbacks.push(callback);

        Ok(())
    }

    fn is_authorized(&self, peer_id: &str) -> bool {
        let mut shared = mutex_lock_unwrap!(self.shared);

        // drain the removals
        let removals = shared.disconnect_receiver.try_iter().collect::<Vec<_>>();
        for peer_id in removals.into_iter() {
            shared.states.remove(&peer_id);
        }

        if let Some(state) = shared.states.get(peer_id) {
            state == &AuthorizationState::Authorized || state == &AuthorizationState::Internal
        } else {
            false
        }
    }
}

struct ManagedAuthorizations {
    states: HashMap<String, AuthorizationState>,
    callbacks: Vec<Box<dyn AuthorizationCallback>>,
    disconnect_receiver: Receiver<String>,
}

impl ManagedAuthorizations {
    fn new(disconnect_receiver: Receiver<String>) -> Self {
        Self {
            states: Default::default(),
            callbacks: Default::default(),
            disconnect_receiver,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PeerAuthorizationState {
    Authorized,
    Unauthorized,
}

/// A callback for changes in a peer's authorization state.
pub trait AuthorizationCallback: Send {
    /// This function is called when a peer's state changes to Authorized or Unauthorized.
    fn on_authorization_change(
        &self,
        peer_id: &str,
        state: PeerAuthorizationState,
    ) -> Result<(), AuthorizationCallbackError>;
}

impl<F> AuthorizationCallback for F
where
    F: Fn(&str, PeerAuthorizationState) -> Result<(), AuthorizationCallbackError> + Send,
{
    fn on_authorization_change(
        &self,
        peer_id: &str,
        state: PeerAuthorizationState,
    ) -> Result<(), AuthorizationCallbackError> {
        (*self)(peer_id, state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use crate::mesh::Mesh;
    use crate::network::Network;
    use crate::transport::{
        ConnectError, Connection, DisconnectError, RecvError, SendError, Transport,
    };

    /// This test runs through the trust authorization state machine happy path. It traverses
    /// through each state, Unknown -> Connecting -> Authorized and verifies that the response
    /// for is_authorized is correct at each stage.
    #[test]
    fn trust_state_machine_valid() {
        let (network, peer_id) = create_network_with_initial_temp_peer();

        let auth_manager = AuthorizationManager::new(network.clone(), "mock_identity".into());

        assert!(!auth_manager.is_authorized(&peer_id));

        assert_eq!(
            Ok(AuthorizationState::Connecting),
            auth_manager.next_state(&peer_id, AuthorizationAction::Connecting)
        );

        assert!(!auth_manager.is_authorized(&peer_id));

        // verify that it cannot be connected again.
        assert_eq!(
            Err(AuthorizationActionError::AlreadyConnecting),
            auth_manager.next_state(&peer_id, AuthorizationAction::Connecting)
        );
        assert!(!auth_manager.is_authorized(&peer_id));

        // Supply the TrustIdentifying action and verify that it is authorized
        let new_peer_id = "abcd".to_string();
        assert_eq!(
            Ok(AuthorizationState::Authorized),
            auth_manager.next_state(
                &peer_id,
                AuthorizationAction::TrustIdentifying(new_peer_id.clone())
            )
        );
        // we no longer have the temp id
        assert!(!auth_manager.is_authorized(&peer_id));
        // but we now have the new identified peer
        assert!(auth_manager.is_authorized(&new_peer_id));
        assert_eq!(vec![new_peer_id.clone()], network.peer_ids());
    }

    /// This test begins a connection, and then unauthorizes the peer.  Verify that the auth
    /// manager reports the correct value for is_authorized, and that the peer is removed.
    #[test]
    fn trust_state_machine_unauthorize_while_connecting() {
        let (network, peer_id) = create_network_with_initial_temp_peer();

        let auth_manager = AuthorizationManager::new(network.clone(), "mock_identity".into());

        assert!(!auth_manager.is_authorized(&peer_id));
        assert_eq!(
            Ok(AuthorizationState::Connecting),
            auth_manager.next_state(&peer_id, AuthorizationAction::Connecting)
        );

        assert_eq!(
            Ok(AuthorizationState::Unauthorized),
            auth_manager.next_state(&peer_id, AuthorizationAction::Unauthorizing)
        );

        assert!(!auth_manager.is_authorized(&peer_id));
        let empty_vec: Vec<String> = Vec::with_capacity(0);
        assert_eq!(empty_vec, network.peer_ids());
    }

    /// This test begins a connection, trusts it, and then unauthorizes the peer.  Verify that
    /// the auth manager reports the correct values for is_authorized, and that the peer is removed.
    #[test]
    fn trust_state_machine_unauthorize_when_authorized() {
        let (network, peer_id) = create_network_with_initial_temp_peer();

        let auth_manager = AuthorizationManager::new(network.clone(), "mock_identity".into());

        assert!(!auth_manager.is_authorized(&peer_id));
        assert_eq!(
            Ok(AuthorizationState::Connecting),
            auth_manager.next_state(&peer_id, AuthorizationAction::Connecting)
        );
        let new_peer_id = "abcd".to_string();
        assert_eq!(
            Ok(AuthorizationState::Authorized),
            auth_manager.next_state(
                &peer_id,
                AuthorizationAction::TrustIdentifying(new_peer_id.clone())
            )
        );
        assert!(!auth_manager.is_authorized(&peer_id));
        assert!(auth_manager.is_authorized(&new_peer_id));
        assert_eq!(vec![new_peer_id.clone()], network.peer_ids());

        assert_eq!(
            Ok(AuthorizationState::Unauthorized),
            auth_manager.next_state(&new_peer_id, AuthorizationAction::Unauthorizing)
        );

        assert!(!auth_manager.is_authorized(&new_peer_id));
        let empty_vec: Vec<String> = Vec::with_capacity(0);
        assert_eq!(empty_vec, network.peer_ids());
    }

    /// This test begins a connection, trust it, and notifies a callback of the authorized state.
    /// It should not be notified of intermediate states.
    #[test]
    fn trust_state_machine_notify_callbacks() {
        let (network, peer_id) = create_network_with_initial_temp_peer();

        let auth_manager = AuthorizationManager::new(network.clone(), "mock_identity".into());
        let notifications = Arc::new(Mutex::new(vec![]));

        let callback_values = notifications.clone();
        auth_manager
            .register_callback(Box::new(
                move |peer_id: &str, state: PeerAuthorizationState| {
                    callback_values
                        .lock()
                        .expect("callback values poisoned")
                        .push((peer_id.to_string(), state.clone()));

                    Ok(())
                },
            ))
            .expect("The callback failed to be registered");

        assert!(!auth_manager.is_authorized(&peer_id));

        assert_eq!(
            Ok(AuthorizationState::Connecting),
            auth_manager.next_state(&peer_id, AuthorizationAction::Connecting)
        );

        assert!(!auth_manager.is_authorized(&peer_id));

        // Supply the TrustIdentifying action and verify that it is authorized
        let new_peer_id = "abcd".to_string();
        assert_eq!(
            Ok(AuthorizationState::Authorized),
            auth_manager.next_state(
                &peer_id,
                AuthorizationAction::TrustIdentifying(new_peer_id.clone())
            )
        );
        // we now have the new identified peer
        assert!(auth_manager.is_authorized(&new_peer_id));
        assert_eq!(vec![new_peer_id.clone()], network.peer_ids());

        assert_eq!(
            Some(("abcd".to_string(), PeerAuthorizationState::Authorized)),
            notifications
                .lock()
                .expect("callback values posioned")
                .pop()
        );
    }

    /// This test verifies that a connection that is authorized, if has disconnected, can begin the
    /// authorization process over again.
    #[test]
    fn disconnection_notification_allows_reauth() {
        let (network, peer_id) = create_network_with_initial_temp_peer();

        let auth_manager = AuthorizationManager::new(network.clone(), "mock_identity".into());
        assert!(!auth_manager.is_authorized(&peer_id));

        assert_eq!(
            Ok(AuthorizationState::Connecting),
            auth_manager.next_state(&peer_id, AuthorizationAction::Connecting)
        );

        assert!(!auth_manager.is_authorized(&peer_id));

        // verify that it cannot be connected again.
        assert_eq!(
            Err(AuthorizationActionError::AlreadyConnecting),
            auth_manager.next_state(&peer_id, AuthorizationAction::Connecting)
        );
        assert!(!auth_manager.is_authorized(&peer_id));

        // Supply the TrustIdentifying action and verify that it is authorized
        let new_peer_id = "abcd".to_string();
        assert_eq!(
            Ok(AuthorizationState::Authorized),
            auth_manager.next_state(
                &peer_id,
                AuthorizationAction::TrustIdentifying(new_peer_id.clone())
            )
        );
        assert!(auth_manager.is_authorized(&new_peer_id));

        // verify that it cannot be connected again.
        assert_eq!(
            Err(AuthorizationActionError::InvalidMessageOrder(
                AuthorizationState::Authorized,
                AuthorizationAction::Connecting
            )),
            auth_manager.next_state(&new_peer_id, AuthorizationAction::Connecting)
        );

        network
            .remove_connection(&new_peer_id)
            .expect("Unable to remove peer");

        // verify that it can be connected again.
        assert_eq!(
            Ok(AuthorizationState::Connecting),
            auth_manager.next_state(&new_peer_id, AuthorizationAction::Connecting)
        );
    }

    fn create_network_with_initial_temp_peer() -> (Network, String) {
        let network = Network::new(Mesh::new(5, 5), 0).unwrap();

        let mut transport = MockConnectingTransport;
        let connection = transport
            .connect("local")
            .expect("Unable to create the connection");

        network
            .add_connection(connection)
            .expect("Unable to add connection to network");

        // We only have one peer, so we can grab this id as the temp id.
        let peer_id = network.peer_ids()[0].clone();

        (network, peer_id)
    }

    struct MockConnectingTransport;

    impl Transport for MockConnectingTransport {
        fn accepts(&self, _: &str) -> bool {
            true
        }

        fn connect(&mut self, _: &str) -> Result<Box<dyn Connection>, ConnectError> {
            Ok(Box::new(MockConnection))
        }

        fn listen(
            &mut self,
            _: &str,
        ) -> Result<Box<dyn crate::transport::Listener>, crate::transport::ListenError> {
            unimplemented!()
        }
    }

    struct MockConnection;

    impl Connection for MockConnection {
        fn send(&mut self, _message: &[u8]) -> Result<(), SendError> {
            Ok(())
        }

        fn recv(&mut self) -> Result<Vec<u8>, RecvError> {
            unimplemented!()
        }

        fn remote_endpoint(&self) -> String {
            String::from("MockConnection")
        }

        fn local_endpoint(&self) -> String {
            String::from("MockConnection")
        }

        fn disconnect(&mut self) -> Result<(), DisconnectError> {
            Ok(())
        }

        fn evented(&self) -> &dyn mio::Evented {
            &MockEvented
        }
    }

    struct MockEvented;

    impl mio::Evented for MockEvented {
        fn register(
            &self,
            _poll: &mio::Poll,
            _token: mio::Token,
            _interest: mio::Ready,
            _opts: mio::PollOpt,
        ) -> std::io::Result<()> {
            Ok(())
        }

        fn reregister(
            &self,
            _poll: &mio::Poll,
            _token: mio::Token,
            _interest: mio::Ready,
            _opts: mio::PollOpt,
        ) -> std::io::Result<()> {
            Ok(())
        }

        fn deregister(&self, _poll: &mio::Poll) -> std::io::Result<()> {
            Ok(())
        }
    }
}
