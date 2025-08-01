// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use assert_matches2::{assert_let, assert_matches};
use insta::assert_json_snapshot;
use matrix_sdk_common::deserialized_responses::{
    AlgorithmInfo, ProcessedToDeviceEvent, ToDeviceUnableToDecryptReason, VerificationLevel,
    VerificationState,
};
use matrix_sdk_test::{async_test, ruma_response_from_json};
use ruma::{
    events::AnyToDeviceEvent, room_id, serde::Raw, to_device::DeviceIdOrAllDevices, RoomId,
    TransactionId,
};
use serde_json::{json, value::to_raw_value, Value};

use crate::{
    machine::{
        test_helpers::{
            build_session_for_pair, get_machine_pair, get_machine_pair_with_session,
            get_prepared_machine_test_helper, send_and_receive_encrypted_to_device_test_helper,
        },
        tests::{self, decryption_verification_state::mark_alice_identity_as_verified_test_helper},
    },
    olm::SenderData,
    types::{
        events::{
            room::encrypted::ToDeviceEncryptedEventContent, EventType as _, ToDeviceCustomEvent,
            ToDeviceEvent,
        },
        requests::ToDeviceRequest,
    },
    utilities::json_convert,
    verification::tests::bob_id,
    CrossSigningBootstrapRequests, DecryptionSettings, DeviceData, EncryptionSettings,
    EncryptionSyncChanges, LocalTrust, OlmError, OlmMachine, Session, TrustRequirement,
};

#[async_test]
async fn test_send_encrypted_to_device() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { raw, encryption_info } = processed_event);

    let decrypted_event = raw.deserialize().unwrap();

    assert_eq!(decrypted_event.event_type().to_string(), custom_event_type.to_owned());

    let decrypted_value = to_raw_value(&raw).unwrap();
    let decrypted_value = serde_json::to_value(decrypted_value).unwrap();

    assert_eq!(
        decrypted_value.get("content").unwrap().get("device_id").unwrap().as_str().unwrap(),
        custom_content.get("device_id").unwrap().as_str().unwrap(),
    );

    assert_eq!(
        decrypted_value.get("content").unwrap().get("rooms").unwrap().as_array().unwrap(),
        custom_content.get("rooms").unwrap().as_array().unwrap(),
    );

    assert_eq!(encryption_info.sender, alice.user_id().to_owned());

    assert_matches!(&encryption_info.sender_device, Some(sender_device));
    assert_eq!(sender_device.to_owned(), alice.device_id().to_owned());

    assert_matches!(
        &encryption_info.algorithm_info,
        AlgorithmInfo::OlmV1Curve25519AesSha2 { curve25519_public_key_base64 }
    );
    let alice_device =
        alice.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();
    assert_eq!(
        curve25519_public_key_base64.to_owned(),
        alice_device.curve25519_key().unwrap().to_base64()
    );

    assert_matches!(
        &encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::UnsignedDevice)
    );
}

#[async_test]
async fn test_receive_custom_encrypted_to_device_fails_if_device_unknown() {
    // When decrypting a custom to device, we expect the recipient to know the
    // sending device. If the device is not known decryption will fail (see
    // `EventError(MissingSigningKey)`). The only exception is room keys where
    // this check can be delayed. This is a reason why there is no test for
    // verification_state `DeviceLinkProblem::MissingDevice`

    let (bob, otk) = get_prepared_machine_test_helper(bob_id(), false).await;

    let alice = OlmMachine::new(tests::alice_id(), tests::alice_device_id()).await;

    let bob_device = DeviceData::from_machine_test_helper(&bob).await.unwrap();
    alice.store().save_device_data(&[bob_device]).await.unwrap();

    let (alice, bob) = build_session_for_pair(alice, bob, otk).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::UnableToDecrypt { utd_info, .. } = processed_event);
    assert_eq!(utd_info.reason, ToDeviceUnableToDecryptReason::DecryptionFailure);
}

#[async_test]
async fn test_excluding_insecure_means_custom_to_device_events_from_unverified_devices_are_utd() {
    // Given we are in "exclude insecure devices" mode
    let decryption_settings = DecryptionSettings {
        sender_device_trust_requirement: TrustRequirement::CrossSignedOrLegacy,
    };

    // Bob is the receiver
    let (bob, otk) = get_prepared_machine_test_helper(bob_id(), false).await;

    // Alice is the sender
    let alice = OlmMachine::new(tests::alice_id(), tests::alice_device_id()).await;

    let bob_device = DeviceData::from_machine_test_helper(&bob).await.unwrap();
    alice.store().save_device_data(&[bob_device]).await.unwrap();

    let (alice, bob) = build_session_for_pair(alice, bob, otk).await;

    // And the receiving device does not consider the sending device verified
    make_alice_unverified(&alice, &bob).await;

    assert!(!bob
        .get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .is_verified());

    // When we send a custom event
    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    // Then it was not processed because the sending device was not verified
    assert_let!(ProcessedToDeviceEvent::UnableToDecrypt { utd_info, .. } = processed_event);

    // And the info provided in the UnableToDecrypt matches what we supplied
    assert_eq!(utd_info.reason, ToDeviceUnableToDecryptReason::UnverifiedSenderDevice);
}

#[async_test]
async fn test_excluding_insecure_does_not_prevent_key_events_being_processed() {
    // Given we are in "exclude insecure devices" mode
    let decryption_settings = DecryptionSettings {
        sender_device_trust_requirement: TrustRequirement::CrossSignedOrLegacy,
    };

    // Bob is the receiver
    let (bob, otk) = get_prepared_machine_test_helper(bob_id(), false).await;

    // Alice is the sender
    let alice = OlmMachine::new(tests::alice_id(), tests::alice_device_id()).await;

    let bob_device = DeviceData::from_machine_test_helper(&bob).await.unwrap();
    alice.store().save_device_data(&[bob_device]).await.unwrap();

    let (alice, bob) = build_session_for_pair(alice, bob, otk).await;

    // And the receiving device does not consider the sending device verified
    make_alice_unverified(&alice, &bob).await;

    assert!(!bob
        .get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .is_verified());

    // When we send a room key event
    let key_event =
        create_and_share_session_without_sender_data(&alice, &bob, room_id!("!23:s.co")).await;

    let key_event_content = serde_json::to_value(&key_event.content).unwrap();

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        "m.room_key",
        &key_event_content,
        &decryption_settings,
    )
    .await;

    // Then it was processed because even though the sending device was not
    // verified, room key events are allowed through.
    assert_matches!(processed_event, ProcessedToDeviceEvent::Decrypted { .. });
}

#[async_test]
async fn test_send_olm_encryption_info_unverified_identity() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    // bootstrap cross-signing
    // tests::setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

    // sign alice device and let bob knows about it
    tests::sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = processed_event);
    assert_eq!(encryption_info.sender, alice.user_id().to_owned());
    assert_eq!(encryption_info.sender_device, Some(alice.device_id().to_owned()));
    assert_eq!(encryption_info.session_id(), None);

    assert_matches!(
        &encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
    );
}

#[async_test]
async fn test_send_olm_encryption_info_verified_identity() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    // bootstrap cross-signing
    tests::setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

    // sign alice device and let bob knows about it
    tests::sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    // Given alice is verified
    mark_alice_identity_as_verified_test_helper(&alice, &bob).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = processed_event);
    assert_eq!(encryption_info.sender, alice.user_id().to_owned());
    assert_eq!(encryption_info.sender_device, Some(alice.device_id().to_owned()));
    assert_eq!(encryption_info.session_id(), None);

    assert_matches!(&encryption_info.verification_state, VerificationState::Verified);
}

#[async_test]
async fn test_send_olm_encryption_info_verified_locally() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    bob.get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .set_local_trust(LocalTrust::Verified)
        .await
        .unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = processed_event);
    assert_eq!(encryption_info.sender, alice.user_id().to_owned());
    assert_eq!(encryption_info.sender_device, Some(alice.device_id().to_owned()));
    assert_eq!(encryption_info.session_id(), None);

    assert_matches!(&encryption_info.verification_state, VerificationState::Verified);
}

#[async_test]
async fn test_send_olm_encryption_info_verification_violation() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    // bootstrap cross-signing
    tests::setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

    // sign alice device and let bob knows about it
    tests::sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    // Given alice is verified
    mark_alice_identity_as_verified_test_helper(&alice, &bob).await;

    // Reset alice identity
    alice.bootstrap_cross_signing(true).await.unwrap();
    tests::setup_cross_signing_for_machine_test_helper(&alice, &bob).await;
    tests::sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = processed_event);
    assert_eq!(encryption_info.sender, alice.user_id().to_owned());
    assert_eq!(encryption_info.sender_device, Some(alice.device_id().to_owned()));
    assert_eq!(encryption_info.session_id(), None);

    assert_matches!(
        &encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::VerificationViolation)
    );
}

#[async_test]
async fn test_processed_to_device_variants() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "rtc.call.encryption_keys";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "call_id": "",
            "keys": [
                {
                    "index": 0,
                    "key": "I7qrSrCR7Yo9B4iGVnR8IA"
                }
            ],
    });

    let device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();
    let raw_encrypted = device
        .encrypt_event_raw(custom_event_type, &custom_content)
        .await
        .expect("Should have encryted the content");

    let request = ToDeviceRequest::new(
        bob.user_id(),
        DeviceIdOrAllDevices::DeviceId(tests::bob_device_id().to_owned()),
        "m.room.encrypted",
        raw_encrypted.cast(),
    );

    let encrypted_event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        tests::to_device_requests_to_content(vec![request.clone().into()]),
    );

    let custom_event = json!({
            "sender": "@alice:example.com",
            "type": "m.new_device",
            "content": {
                "device_id": "XYZABCDE",
                "rooms": ["!726s6s6q:example.com"]
            }
    });
    let clear_event = serde_json::from_value::<ToDeviceCustomEvent>(custom_event.clone()).unwrap();

    let encrypted_event = json_convert(&encrypted_event).unwrap();
    let clear_event = json_convert(&clear_event).unwrap();

    let malformed_event_no_type = json!({
        "sender": "@alice:example.com",
        "content": {
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
        }
    });
    let malformed_event: Raw<AnyToDeviceEvent> =
        serde_json::from_value(malformed_event_no_type).unwrap();

    let alice_curve = alice
        .get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .curve25519_key()
        .unwrap();
    let bob_curve = bob
        .get_device(bob.user_id(), bob.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .curve25519_key()
        .unwrap();

    // let's add a UTD event
    let utd_event = json!({
        "content": {
            "algorithm": "m.olm.v1.curve25519-aes-sha2",
            "ciphertext": {
                bob_curve.to_base64(): {
                    // this payload is just captured from a sync of some other element web with other users
                    "body": "Awogjvpx458CGhuo77HX/+tp1sxgRoCi7iAlzMvfrpbWoREQAiKACysX/p+ojr5QitCi9WRXNyamW2v2LTvoyWKtVaA2oHnYGR5s5RYhDfnIgh5MMSqqKlAbfqLvrbLovTYcKagCBbFnbA43f6zYM44buGgy8q70hMVH5WP6aK1E9Z3DVZ+8PnXQGpsrxvz2IsL6w0Nzl/qUyBEQFcgkjoDPawbsZRCllMgq2LQUyqlun6IgDTCozqsfxhDWpdfYGde4z16m34Ang7f5pH+BmPrFs6E1AO5+UbhhhS6NwWlfEtA6/9yfMxWLz1d2OrLh+QG7lYFAU9/CzIoPxaHKKr4JxgL9CjsmUPyDymWOWHP0jLi1NwpOv6hGpx0FgM7jJIMk6gWGgC5rEgEeTIwdrJh3F9OKTNSva5hvD9LomGk6tZgzQG6oap1e3wiOUyTt6S7BlyMppIu3RlIiNihZ9e17JEGiGDXOXzMJ6ISAgvGVgTP7+EvyEt2Wt4du7uBo/UvljRvVNu3I8tfItizPAOlvz460+aBDxk+sflJWt7OnhiyPnOCfopb+1RzqKVCnnPyVaP2f4BPf8qpn/f5YZk+5jJgBrGPiHzzmb3sQ5pC470s6+U3MpVFlFTG/xPBtMRMwPsbKoHfnRPqIqGu5dQ1Sw7T6taDXWjP450TvjxgHK5t2z1rLA2SXzAB1P8xbi6YXqQwxL6PvMNHn/TM0jiIQHYuqg5/RKLyhHybfP8JAjgNBw9z16wfKR/YoYFr7c+S4McQaMNa8v2SxGzhpCC3duAoK2qCWLEkYRO5cMCsGm/9bf8Q+//OykygBU/hdkT1eHUbexgALPLdfhzduutU7pbChg4T7SH7euh/3NLmS/SQvkmPfm3ckbh/Vlcj9CsXws/7MX/VJbhpbyzgBNtMnbG6tAeAofMa6Go/yMgiNBZIhLpAm31iUbUhaGm2IIlF/lsmSYEiBPoSVfFU44tetX2I/PBDGiBlzyU+yC2TOEBwMGxBE3WHbIe5/7sKW8xJF9t+HBfxIyW1QRtY3EKdEcuVOTyMxYzq3L5OKOOtPDHObYiiXg00mAgdQqgfkEAIfoRCOa2NYfTedwwo0S77eQ1sPvW5Hhf+Cm+bLibkWzaYHEZF+vyE9/Tn0tZGtH07RXfUyhp1vtTH49OBZHGkb/r+L8OjYJTST1dDCGqeGXO3uwYjoWHXtezLVHYgL+UOwcLJfMF5s9DQiqcfYXzp2kEWGsaetBFXcUWqq4RMHqlr6QfbxyuYLlQzc/AYA/MrT3J6nDpNLcvozH3RcIs8NcKcjdtjvgL0QGThy3RcecJQEDx3STrkkePL3dlyFCtVsmtQ0vjBBCxUgdySfxiobGGnpezZYi7q+Xz61GOZ9QqYmkcZOPzfNWeqtmzB7gqlH1gkFsK2yMAzKq2XCDFHvA7YAT3yMGiY06FcQ+2jyg7Bk2Q+AvjTG8hlPlmt6BZfW5cz1qx1apQn1qHXHrgfWcI52rApYQlNPOU1Uc8kZ8Ee6XUhhXBGY1rvZiKjKFG0PPuS8xo4/P7/u+gH5gItmEVDFL6giYPFsPpqAQkUN7hFoGiVZEjO4PwrLOmydsEcNOfACqrnUs08FQtvPg0sjHnxh6nh6FUQv93ukKl6+c9d+pCsN2xukrQ7Dog3nrjFZ6PrS5J0k9rDAOwTB55sfGXPZ2rATOK1WS4XcpsCtqwnYm4sGNc8ALMQkQ97zCnw8TcQwLvdUMlfbqQ5ykDQpQD68fITEDDHmBAeTCjpC713E6AhvOMwTJvjhd7hSkeOTRTmn9zXIVGNo1jSr8u0xO9uLGeWsV0+UlRLgp7/nsgfermjwNN8wj6MW3DHGS8UzzYfe9TGCeywqqIUTqgfXY48leGgB7twh4cl4jcOQniLATTvigIAQIvq/Uv8L45BGnkpKTdQ5F73gehXdVA",
                    "type": 1
                }
            },
            "org.matrix.msgid": "93ee0170aa7740d0ac9ee137e820302d",
            "sender_key": alice_curve.to_base64(),
        },
        "type": "m.room.encrypted",
        "sender": "@alice:example.org",
    });

    let utd_event: Raw<AnyToDeviceEvent> = serde_json::from_value(utd_event).unwrap();

    let sync_changes = EncryptionSyncChanges {
        to_device_events: vec![encrypted_event, clear_event, malformed_event, utd_event],
        changed_devices: &Default::default(),
        one_time_keys_counts: &Default::default(),
        unused_fallback_keys: None,
        next_batch_token: None,
    };

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let (processed, _) =
        bob.receive_sync_changes(sync_changes, &decryption_settings).await.unwrap();

    assert_eq!(4, processed.len());

    let processed_event = &processed[0];
    assert_matches!(processed_event, ProcessedToDeviceEvent::Decrypted { .. });

    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        assert_json_snapshot!(
            processed_event.to_raw().deserialize_as::<Value>().unwrap(),
             {
                 ".keys.ed25519" => "[sender_ed25519_key]",
                 r#"["sender_device_keys"].device_id"# => "[ABCDEFGH]",
                 r#"["sender_device_keys"].keys"# => "++REDACTED++",
                 r#"["sender_device_keys"].signatures"# => "++REDACTED++",
                 // Redacted because depending on feature flags
                 r#"["sender_device_keys"].algorithms"# => "++REDACTED++",
                 ".recipient_keys.ed25519" => "[recipient_sender_key]",
            }
        );
    });

    let processed_event = &processed[1];
    assert_matches!(processed_event, ProcessedToDeviceEvent::PlainText(_));

    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        assert_json_snapshot!(
            processed_event.to_raw().deserialize_as::<Value>().unwrap(),
        );
    });

    let processed_event = &processed[2];
    assert_matches!(processed_event, ProcessedToDeviceEvent::Invalid(_));

    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        assert_json_snapshot!(
            processed_event.to_raw().deserialize_as::<Value>().unwrap(),
        );
    });

    let processed_event = &processed[3];
    assert_matches!(processed_event, ProcessedToDeviceEvent::UnableToDecrypt { utd_info, .. });
    assert_eq!(utd_info.reason, ToDeviceUnableToDecryptReason::DecryptionFailure);

    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        assert_json_snapshot!(
            processed_event.to_raw().deserialize_as::<Value>().unwrap(),
            {
                ".content.sender_key" => "[sender_ed25519_key]",
                ".content.ciphertext" => "[++REDACTED++]",
            }
        );
    });
}

#[async_test]
async fn test_send_encrypted_to_device_no_session() {
    let (alice, bob, _) = get_machine_pair(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let encryption_result = alice
        .get_device(bob.user_id(), tests::bob_device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .encrypt_event_raw(custom_event_type, &custom_content)
        .await;

    assert_matches!(encryption_result, Err(OlmError::MissingSession));
}

/// Create a new [`OutboundGroupSession`], and build a to-device event to share
/// it with another [`OlmMachine`], *without* sending the MSC4147 sender data.
///
/// # Arguments
///
/// * `alice` - sending device.
/// * `bob` - receiving device.
/// * `room_id` - room to create a session for.
async fn create_and_share_session_without_sender_data(
    alice: &OlmMachine,
    bob: &OlmMachine,
    room_id: &RoomId,
) -> ToDeviceEvent<ToDeviceEncryptedEventContent> {
    let (outbound_session, _) = alice
        .inner
        .group_session_manager
        .get_or_create_outbound_session(
            room_id,
            EncryptionSettings::default(),
            SenderData::unknown(),
        )
        .await
        .unwrap();

    // In future, we might want to save the session to the store, to better match
    // the behaviour of the real implementation. See
    // `GroupSessionManager::share_room_key` for inspiration on how to do that.

    let olm_sessions = alice
        .store()
        .get_sessions(&bob.identity_keys().curve25519.to_base64())
        .await
        .unwrap()
        .unwrap();
    let mut olm_session: Session = olm_sessions.lock().await[0].clone();

    let room_key_content = outbound_session.as_content().await;
    let plaintext = serde_json::to_string(&json!({
        "sender": alice.user_id(),
        "sender_device": alice.device_id(),
        "keys": { "ed25519": alice.identity_keys().ed25519.to_base64() },
        // We deliberately do *not* include:
        // "org.matrix.msc4147.device_keys": alice_device_keys,
        "recipient": bob.user_id(),
        "recipient_keys": { "ed25519": bob.identity_keys().ed25519.to_base64() },
        "type": room_key_content.event_type(),
        "content": room_key_content,
    }))
    .unwrap();

    let ciphertext = olm_session.encrypt_helper(&plaintext).await;
    ToDeviceEvent::new(
        alice.user_id().to_owned(),
        olm_session.build_encrypted_event(ciphertext, None).await.unwrap(),
    )
}

/// Simulate uploading keys for alice that mean bob thinks alice's device
/// exists, but is unverified.
async fn make_alice_unverified(alice: &OlmMachine, bob: &OlmMachine) {
    let CrossSigningBootstrapRequests { upload_signing_keys_req: upload_signing, .. } =
        alice.bootstrap_cross_signing(false).await.expect("Expect Alice x-signing key request");

    let device_keys = alice
        .get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .as_device_keys()
        .to_owned();

    let updated_keys_with_x_signing = json!({ device_keys.device_id.to_string(): device_keys });

    let json = json!({
        "device_keys": {
            alice.user_id() : updated_keys_with_x_signing
        },
        "failures": {},
        "master_keys": {
            alice.user_id() : upload_signing.master_key.unwrap(),
        },
        "user_signing_keys": {
            alice.user_id() : upload_signing.user_signing_key.unwrap(),
        },
        "self_signing_keys": {
            alice.user_id() : upload_signing.self_signing_key.unwrap(),
        },
      }
    );

    let kq_response = ruma_response_from_json(&json);
    alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
}
