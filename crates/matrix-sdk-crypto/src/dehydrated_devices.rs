// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Submodule for device dehydration support.
//!
//! Dehydrated devices intend to solve the use-case where users might want to
//! frequently delete their device, in which case other users won't be able to
//! send end-to-end encrypted messages to them as no device exists to receive
//! and decrypt them.
//!
//! A dehydrated device is a kind-of omnipresent virtual device that lives on
//! the homeserver. A dehydrated device acts as a normal device from the
//! point of view of other devices. It uploads device and one-time keys to
//! the homeserver which other devices can download and start 1-to-1 encrypted
//! sessions with the device just like with any other device.
//!
//! The one important difference is that the private parts of the uploaded
//! device and one-time keys are encrypted and uploaded to the homeserver as
//! well.
//!
//! Once the user creates a new real device, the real device can download the
//! private keys of the dehydrated device from the homeserver, decrypt them and
//! download all the encrypted to-device events the dehydrated device has
//! received. This process is called rehydration.
//!
//! After the rehydration process is completed, the user's real device should
//! create a new dehydrated device.

// TODO: Once a device has been rehydrated it might need to download and decrypt
// a lot of to-device events. This process might take some time and we should
// support resuming it.

use std::sync::Arc;

use ruma::{
    api::client::dehydrated_device::{put_dehydrated_device, DehydratedDeviceData},
    assign,
    events::AnyToDeviceEvent,
    serde::Raw,
    DeviceId,
};
use thiserror::Error;
use tracing::{instrument, trace};
use vodozemac::{DehydratedDeviceError, LibolmPickleError};

use crate::{
    store::{
        types::{Changes, DehydratedDeviceKey, RoomKeyInfo},
        CryptoStoreWrapper, MemoryStore, Store,
    },
    verification::VerificationMachine,
    Account, CryptoStoreError, DecryptionSettings, EncryptionSyncChanges, OlmError, OlmMachine,
    SignatureError,
};

/// Error type for device dehydration issues.
#[derive(Debug, Error)]
pub enum DehydrationError {
    /// The legacy dehydrated device could not be unpickled.
    #[error(transparent)]
    LegacyPickle(#[from] LibolmPickleError),

    /// The dehydrated device could not be unpickled.
    #[error(transparent)]
    Pickle(#[from] DehydratedDeviceError),

    /// The pickle key has an invalid length
    #[error("The pickle key has an invalid length, expected 32 bytes, got {0}")]
    PickleKeyLength(usize),

    /// The dehydrated device could not be signed by our user identity,
    /// we're missing the self-signing key.
    #[error("The self-signing key is missing, can't create a dehydrated device")]
    MissingSigningKey(#[from] SignatureError),

    /// We could not deserialize the dehydrated device data.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// The store ran into an error.
    #[error(transparent)]
    Store(#[from] CryptoStoreError),
}

/// Struct collecting methods to create and rehydrate dehydrated devices.
#[derive(Debug)]
pub struct DehydratedDevices {
    pub(crate) inner: OlmMachine,
}

impl DehydratedDevices {
    /// Create a new [`DehydratedDevice`] which can be uploaded to the server.
    pub async fn create(&self) -> Result<DehydratedDevice, DehydrationError> {
        let user_id = self.inner.user_id();
        let user_identity = self.inner.store().private_identity();

        let account = Account::new_dehydrated(user_id);
        let store =
            Arc::new(CryptoStoreWrapper::new(user_id, account.device_id(), MemoryStore::new()));

        let verification_machine = VerificationMachine::new(
            account.static_data().clone(),
            user_identity.clone(),
            store.clone(),
        );

        let store =
            Store::new(account.static_data().clone(), user_identity, store, verification_machine);
        store
            .save_pending_changes(crate::store::types::PendingChanges { account: Some(account) })
            .await?;

        Ok(DehydratedDevice { store })
    }

    /// Rehydrate the dehydrated device.
    ///
    /// Once rehydrated, to-device events can be pushed into the
    /// [`RehydratedDevice`] to collect the room keys the device has
    /// received.
    ///
    /// For more info see the example for the
    /// [`RehydratedDevice::receive_events()`] method.
    ///
    /// # Arguments
    ///
    /// * `pickle_key` - The encryption key that was used to encrypt the private
    ///   parts of the identity keys, and one-time keys of the device.
    ///
    /// * `device_id` - The unique identifier of the device.
    ///
    /// * `device_data` - The encrypted data of the device, containing the
    ///   private keys of the device.
    pub async fn rehydrate(
        &self,
        pickle_key: &DehydratedDeviceKey,
        device_id: &DeviceId,
        device_data: Raw<DehydratedDeviceData>,
    ) -> Result<RehydratedDevice, DehydrationError> {
        let rehydrated =
            self.inner.rehydrate(pickle_key.inner.as_ref(), device_id, device_data).await?;

        Ok(RehydratedDevice { rehydrated, original: self.inner.to_owned() })
    }

    /// Get the cached dehydrated device pickle key if any.
    ///
    /// None if the key was not previously cached (via
    /// [`DehydratedDevices::save_dehydrated_device_pickle_key`]).
    ///
    /// Should be used to periodically rotate the dehydrated device to avoid
    /// one-time keys exhaustion and accumulation of to_device messages.
    pub async fn get_dehydrated_device_pickle_key(
        &self,
    ) -> Result<Option<DehydratedDeviceKey>, DehydrationError> {
        Ok(self.inner.store().load_dehydrated_device_pickle_key().await?)
    }

    /// Store the dehydrated device pickle key in the crypto store.
    ///
    /// This is useful if the client wants to periodically rotate dehydrated
    /// devices to avoid one-time keys exhaustion and accumulated to_device
    /// problems.
    pub async fn save_dehydrated_device_pickle_key(
        &self,
        dehydrated_device_pickle_key: &DehydratedDeviceKey,
    ) -> Result<(), DehydrationError> {
        let changes = Changes {
            dehydrated_device_pickle_key: Some(dehydrated_device_pickle_key.clone()),
            ..Default::default()
        };
        Ok(self.inner.store().save_changes(changes).await?)
    }

    /// Deletes the previously stored dehydrated device pickle key.
    pub async fn delete_dehydrated_device_pickle_key(&self) -> Result<(), DehydrationError> {
        Ok(self.inner.store().delete_dehydrated_device_pickle_key().await?)
    }
}

/// A rehydraded device.
///
/// This device can now receive to-device events to decrypt and gather room keys
/// which were sent to the dehydrated device.
#[derive(Debug)]
pub struct RehydratedDevice {
    rehydrated: OlmMachine,
    original: OlmMachine,
}

impl RehydratedDevice {
    /// Feed to-device events the device was supposed to receive into the
    /// [`RehydratedDevice`].
    ///
    /// Most to-device events we feed into the [`RehydratedDevice`] will contain
    /// room keys, the rehydrated device will pass these room keys into our
    /// own [`OlmMachine`] which will persist them and make the room keys
    /// available for use using the usual
    /// [`OlmMachine::decrypt_room_event()`] method.
    ///
    /// Once the homeserver returns a response without any to-device events, we
    /// can safely delete the current dehydrated device and create a new one.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use anyhow::Result;
    /// # use matrix_sdk_crypto::{
    ///     DecryptionSettings, OlmMachine, TrustRequirement, store::types::DehydratedDeviceKey
    /// };
    /// # use ruma::{api::client::dehydrated_device, DeviceId};
    /// # async fn example() -> Result<()> {
    /// # let machine: OlmMachine = unimplemented!();
    /// async fn get_dehydrated_device() -> Result<dehydrated_device::get_dehydrated_device::unstable::Response> {
    ///     todo!("Download the dehydrated device");
    /// }
    ///
    /// async fn get_events(
    ///     device_id: &DeviceId,
    ///     since_token: Option<&str>
    /// ) -> Result<dehydrated_device::get_events::unstable::Response> {
    ///     todo!("Download the to-device events of the dehydrated device");
    /// }
    /// // Get the cached dehydrated key (got it after verification/recovery)
    /// let pickle_key = machine
    ///     .dehydrated_devices().get_dehydrated_device_pickle_key().await?.unwrap();
    ///
    /// // Fetch the dehydrated device from the server.
    /// let response = get_dehydrated_device().await?;
    /// let device_id = response.device_id;
    ///
    /// // Rehydrate the device.
    /// let rehydrated = machine
    ///     .dehydrated_devices()
    ///     .rehydrate(&pickle_key, &device_id, response.device_data)
    ///     .await?;
    ///
    /// let mut since_token = None;
    /// let mut imported_room_keys = 0;
    /// let decryption_settings = DecryptionSettings {
    ///     sender_device_trust_requirement: TrustRequirement::Untrusted
    /// };
    ///
    /// loop {
    ///     let response =
    ///         get_events(&device_id, since_token).await?;
    ///
    ///     if response.events.is_empty() {
    ///         break;
    ///     }
    ///
    ///     since_token = response.next_batch.as_deref();
    ///     imported_room_keys += rehydrated.receive_events(response.events, &decryption_settings).await?.len();
    /// }
    ///
    /// println!("Successfully imported {imported_room_keys} from the dehydrated device.");
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(
        skip_all,
        fields(
            user_id = ?self.original.user_id(),
            rehydrated_device_id = ?self.rehydrated.device_id(),
            original_device_id = ?self.original.device_id()
        )
    )]
    pub async fn receive_events(
        &self,
        events: Vec<Raw<AnyToDeviceEvent>>,
        decryption_settings: &DecryptionSettings,
    ) -> Result<Vec<RoomKeyInfo>, OlmError> {
        trace!("Receiving events for a rehydrated Device");

        let sync_changes = EncryptionSyncChanges {
            to_device_events: events,
            next_batch_token: None,
            one_time_keys_counts: &Default::default(),
            changed_devices: &Default::default(),
            unused_fallback_keys: None,
        };

        // Let us first give the events to the rehydrated device, this will decrypt any
        // encrypted to-device events and fetch out the room keys.
        let mut rehydrated_transaction = self.rehydrated.store().transaction().await;

        let (_, changes) = self
            .rehydrated
            .preprocess_sync_changes(&mut rehydrated_transaction, sync_changes, decryption_settings)
            .await?;

        // Now take the room keys and persist them in our original `OlmMachine`.
        let room_keys = &changes.inbound_group_sessions;
        let updates = room_keys.iter().map(Into::into).collect();

        trace!(room_key_count = room_keys.len(), "Collected room keys from the rehydrated device");

        self.original.store().save_inbound_group_sessions(room_keys).await?;

        rehydrated_transaction.commit().await?;
        self.rehydrated.store().save_changes(changes).await?;

        Ok(updates)
    }
}

/// A dehydrated device that can uploaded to the homeserver.
///
/// To upload the dehydrated device take a look at the
/// [`DehydratedDevice::keys_for_upload()`] method.
#[derive(Debug)]
pub struct DehydratedDevice {
    store: Store,
}

impl DehydratedDevice {
    /// Get the request to upload the dehydrated device.
    ///
    /// # Arguments
    ///
    /// * `initial_device_display_name` - The human-readable name this device
    ///   should have.
    /// * `pickle_key` - The encryption key that should be used to encrypt the
    ///   private parts of the identity keys, and one-time keys of the device.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk_crypto::OlmMachine;    /// #
    /// use matrix_sdk_crypto::store::types::DehydratedDeviceKey;
    ///
    /// async fn example() -> anyhow::Result<()> {
    /// # let machine: OlmMachine = unimplemented!();
    /// // Create a new random key
    /// let pickle_key = DehydratedDeviceKey::new()?;
    ///
    /// // Create the dehydrated device.
    /// let device = machine.dehydrated_devices().create().await?;
    ///
    /// // Create the request that should upload the device.
    /// let request = device
    ///     .keys_for_upload("Dehydrated device".to_owned(), &pickle_key)
    ///     .await?;
    ///
    /// // Save the key if you want to later one rotate the dehydrated device
    /// machine.dehydrated_devices().save_dehydrated_device_pickle_key(&pickle_key).await.unwrap();
    ///
    /// // Send the request out using your HTTP client.
    /// // client.send(request).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(
        skip_all, fields(
            user_id = ?self.store.static_account().user_id,
            device_id = ?self.store.static_account().device_id,
            identity_keys = ?self.store.static_account().identity_keys,
        )
    )]
    pub async fn keys_for_upload(
        &self,
        initial_device_display_name: String,
        pickle_key: &DehydratedDeviceKey,
    ) -> Result<put_dehydrated_device::unstable::Request, DehydrationError> {
        let mut transaction = self.store.transaction().await;

        let account = transaction.account().await?;
        account.generate_fallback_key_if_needed();

        let (device_keys, one_time_keys, fallback_keys) = account.keys_for_upload();

        let mut device_keys = device_keys
            .expect("We should always try to upload device keys for a dehydrated device.");

        self.store.private_identity().lock().await.sign_device_keys(&mut device_keys).await?;

        trace!("Creating an upload request for a dehydrated device");

        let device_id = self.store.static_account().device_id.clone();
        let device_data = account.dehydrate(pickle_key.inner.as_ref());
        let initial_device_display_name = Some(initial_device_display_name);

        transaction.commit().await?;

        Ok(
            assign!(put_dehydrated_device::unstable::Request::new(device_id, device_data, device_keys.to_raw()), {
                one_time_keys, fallback_keys, initial_device_display_name
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, iter};

    use js_option::JsOption;
    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::{
            dehydrated_device::put_dehydrated_device,
            keys::get_keys::v3::Response as KeysQueryResponse,
        },
        assign,
        encryption::DeviceKeys,
        events::AnyToDeviceEvent,
        room_id,
        serde::Raw,
        user_id, DeviceId, RoomId, TransactionId, UserId,
    };

    use crate::{
        dehydrated_devices::DehydratedDevice,
        machine::{
            test_helpers::{create_session, get_prepared_machine_test_helper},
            tests::to_device_requests_to_content,
        },
        olm::OutboundGroupSession,
        store::types::DehydratedDeviceKey,
        types::{events::ToDeviceEvent, DeviceKeys as DeviceKeysType},
        utilities::json_convert,
        DecryptionSettings, EncryptionSettings, OlmMachine, TrustRequirement,
    };

    fn pickle_key() -> DehydratedDeviceKey {
        DehydratedDeviceKey::from_bytes(&[0u8; 32])
    }

    fn user_id() -> &'static UserId {
        user_id!("@alice:localhost")
    }

    async fn get_olm_machine() -> OlmMachine {
        let (olm_machine, _) = get_prepared_machine_test_helper(user_id(), false).await;
        olm_machine.bootstrap_cross_signing(false).await.unwrap();

        olm_machine
    }

    // Insert some device keys into a [`OlmMachine`] making the [`Device`] available
    // to the [`OlmMachine`].
    async fn receive_device_keys(
        olm_machine: &OlmMachine,
        user_id: &UserId,
        device_id: &DeviceId,
        device_keys: Raw<DeviceKeys>,
    ) {
        let device_keys = BTreeMap::from([(device_id.to_owned(), device_keys)]);

        let keys_query_response = assign!(
            KeysQueryResponse::new(), {
                device_keys: BTreeMap::from([(user_id.to_owned(), device_keys)]),
            }
        );

        olm_machine
            .mark_request_as_sent(&TransactionId::new(), &keys_query_response)
            .await
            .unwrap();
    }

    async fn send_room_key(
        machine: &OlmMachine,
        room_id: &RoomId,
        recipient: &UserId,
    ) -> (Raw<AnyToDeviceEvent>, OutboundGroupSession) {
        let to_device_requests = machine
            .share_room_key(room_id, iter::once(recipient), EncryptionSettings::default())
            .await
            .unwrap();

        let event = ToDeviceEvent::new(
            user_id().to_owned(),
            to_device_requests_to_content(to_device_requests),
        );

        let session =
            machine.inner.group_session_manager.get_outbound_group_session(room_id).expect(
                "An outbound group session should have been created when the room key was shared",
            );

        (
            json_convert(&event)
                .expect("We should be able to convert the to-device event into it's Raw variatn"),
            session,
        )
    }

    #[async_test]
    async fn test_dehydrated_device_creation() {
        let olm_machine = get_olm_machine().await;

        let dehydrated_device = olm_machine.dehydrated_devices().create().await.unwrap();

        let request = dehydrated_device
            .keys_for_upload("Foo".to_owned(), &pickle_key())
            .await
            .expect("We should be able to create a request to upload a dehydrated device");

        assert!(
            !request.one_time_keys.is_empty(),
            "The dehydrated device creation request should contain some one-time keys"
        );

        assert!(
            !request.fallback_keys.is_empty(),
            "The dehydrated device creation request should contain some fallback keys"
        );

        let device_keys: DeviceKeysType = request.device_keys.deserialize_as().unwrap();
        assert_eq!(
            device_keys.dehydrated,
            JsOption::Some(true),
            "The device keys of the dehydrated device should be marked as dehydrated."
        );
    }

    #[async_test]
    async fn test_dehydrated_device_rehydration() {
        let room_id = room_id!("!test:example.org");
        let alice = get_olm_machine().await;

        let dehydrated_device = alice.dehydrated_devices().create().await.unwrap();

        let mut request = dehydrated_device
            .keys_for_upload("Foo".to_owned(), &pickle_key())
            .await
            .expect("We should be able to create a request to upload a dehydrated device");

        let (key_id, one_time_key) = request
            .one_time_keys
            .pop_first()
            .expect("The dehydrated device creation request should contain a one-time key");

        // Ensure that we know about the public keys of the dehydrated device.
        receive_device_keys(&alice, user_id(), &request.device_id, request.device_keys).await;
        // Create a 1-to-1 Olm session with the dehydrated device.
        create_session(&alice, user_id(), &request.device_id, key_id, one_time_key).await;

        // Send a room key to the dehydrated device.
        let (event, group_session) = send_room_key(&alice, room_id, user_id()).await;

        // Let's now create a new `OlmMachine` which doesn't know about the room key.
        let bob = get_olm_machine().await;

        let room_key = bob
            .store()
            .get_inbound_group_session(room_id, group_session.session_id())
            .await
            .unwrap();

        assert!(
            room_key.is_none(),
            "We should not have access to the room key that was only sent to the dehydrated device"
        );

        // Rehydrate the device.
        let rehydrated = bob
            .dehydrated_devices()
            .rehydrate(&pickle_key(), &request.device_id, request.device_data)
            .await
            .expect("We should be able to rehydrate the device");

        assert_eq!(rehydrated.rehydrated.device_id(), request.device_id);
        assert_eq!(rehydrated.original.device_id(), alice.device_id());

        let decryption_settings =
            DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

        // Push the to-device event containing the room key into the rehydrated device.
        let ret = rehydrated
            .receive_events(vec![event], &decryption_settings)
            .await
            .expect("We should be able to push to-device events into the rehydrated device");

        assert_eq!(ret.len(), 1, "The rehydrated device should have imported a room key");

        // The `OlmMachine` now does know about the room key since the rehydrated device
        // shared it with us.
        let room_key = bob
            .store()
            .get_inbound_group_session(room_id, group_session.session_id())
            .await
            .unwrap()
            .expect("We should now have access to the room key, since the rehydrated device imported it for us");

        assert_eq!(
            room_key.session_id(),
            group_session.session_id(),
            "The session ids of the imported room key and the outbound group session should match"
        );
    }

    #[async_test]
    async fn test_dehydrated_device_pickle_key_cache() {
        let alice = get_olm_machine().await;

        let dehydrated_manager = alice.dehydrated_devices();

        let stored_key = dehydrated_manager.get_dehydrated_device_pickle_key().await.unwrap();
        assert!(stored_key.is_none());

        let pickle_key = DehydratedDeviceKey::new().unwrap();

        dehydrated_manager.save_dehydrated_device_pickle_key(&pickle_key).await.unwrap();

        let stored_key =
            dehydrated_manager.get_dehydrated_device_pickle_key().await.unwrap().unwrap();
        assert_eq!(stored_key.to_base64(), pickle_key.to_base64());

        let dehydrated_device = dehydrated_manager.create().await.unwrap();

        let request = dehydrated_device
            .keys_for_upload("Foo".to_owned(), &stored_key)
            .await
            .expect("We should be able to create a request to upload a dehydrated device");

        // Rehydrate the device.
        dehydrated_manager
            .rehydrate(&stored_key, &request.device_id, request.device_data)
            .await
            .expect("We should be able to rehydrate the device");

        dehydrated_manager
            .delete_dehydrated_device_pickle_key()
            .await
            .expect("Should be able to delete the dehydrated device key");

        let stored_key = dehydrated_manager.get_dehydrated_device_pickle_key().await.unwrap();
        assert!(stored_key.is_none());
    }

    /// Test that we can rehydrate an older version of dehydrated device
    #[async_test]
    async fn test_legacy_dehydrated_device_rehydration() {
        let room_id = room_id!("!test:example.org");
        let alice = get_olm_machine().await;

        let dehydrated_device = alice.dehydrated_devices().create().await.unwrap();
        let mut request =
            legacy_dehydrated_device_keys_for_upload(&dehydrated_device, &pickle_key()).await;

        let (key_id, one_time_key) = request
            .one_time_keys
            .pop_first()
            .expect("The dehydrated device creation request should contain a one-time key");

        let device_id = request.device_id;

        // Ensure that we know about the public keys of the dehydrated device.
        receive_device_keys(&alice, user_id(), &device_id, request.device_keys).await;
        // Create a 1-to-1 Olm session with the dehydrated device.
        create_session(&alice, user_id(), &device_id, key_id, one_time_key).await;

        // Send a room key to the dehydrated device.
        let (event, group_session) = send_room_key(&alice, room_id, user_id()).await;

        // Let's now create a new `OlmMachine` which doesn't know about the room key.
        let bob = get_olm_machine().await;

        let room_key = bob
            .store()
            .get_inbound_group_session(room_id, group_session.session_id())
            .await
            .unwrap();

        assert!(
            room_key.is_none(),
            "We should not have access to the room key that was only sent to the dehydrated device"
        );

        // Rehydrate the device.
        let rehydrated = bob
            .dehydrated_devices()
            .rehydrate(&pickle_key(), &device_id, request.device_data)
            .await
            .expect("We should be able to rehydrate the device");

        assert_eq!(rehydrated.rehydrated.device_id(), &device_id);
        assert_eq!(rehydrated.original.device_id(), alice.device_id());

        let decryption_settings =
            DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

        // Push the to-device event containing the room key into the rehydrated device.
        let ret = rehydrated
            .receive_events(vec![event], &decryption_settings)
            .await
            .expect("We should be able to push to-device events into the rehydrated device");

        assert_eq!(ret.len(), 1, "The rehydrated device should have imported a room key");

        // The `OlmMachine` now does know about the room key since the rehydrated device
        // shared it with us.
        let room_key = bob
            .store()
            .get_inbound_group_session(room_id, group_session.session_id())
            .await
            .unwrap()
            .expect("We should now have access to the room key, since the rehydrated device imported it for us");

        assert_eq!(
            room_key.session_id(),
            group_session.session_id(),
            "The session ids of the imported room key and the outbound group session should match"
        );
    }

    /// Duplicates the behaviour of [`DehydratedDevice::keys_for_upload`],
    /// except that it calls [`Account::legacy_dehydrate`] instead of
    /// [`Account::dehydrate`].
    async fn legacy_dehydrated_device_keys_for_upload(
        dehydrated_device: &DehydratedDevice,
        pickle_key: &DehydratedDeviceKey,
    ) -> put_dehydrated_device::unstable::Request {
        let mut transaction = dehydrated_device.store.transaction().await;
        let account = transaction.account().await.unwrap();
        account.generate_fallback_key_if_needed();

        let (device_keys, one_time_keys, fallback_keys) = account.keys_for_upload();
        let mut device_keys = device_keys.unwrap();
        dehydrated_device
            .store
            .private_identity()
            .lock()
            .await
            .sign_device_keys(&mut device_keys)
            .await
            .expect("Should be able to cross-sign a device");

        let device_id = account.device_id().to_owned();
        let device_data = account.legacy_dehydrate(pickle_key.inner.as_ref());
        transaction.commit().await.unwrap();

        assign!(put_dehydrated_device::unstable::Request::new(device_id, device_data, device_keys.to_raw()), {
            one_time_keys, fallback_keys
        })
    }
}
