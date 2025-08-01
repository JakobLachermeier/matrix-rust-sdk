// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
// Copyright 2022 Kévin Commaille
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

use futures_core::Stream;
use futures_util::{stream, StreamExt};
use matrix_sdk_base::{
    media::{MediaFormat, MediaRequestParameters},
    store::StateStoreExt,
    StateStoreDataKey, StateStoreDataValue,
};
use mime::Mime;
use ruma::{
    api::client::{
        account::{
            add_3pid, change_password, deactivate, delete_3pid, get_3pids,
            request_3pid_management_token_via_email, request_3pid_management_token_via_msisdn,
        },
        config::{get_global_account_data, set_global_account_data},
        error::ErrorKind,
        profile::{
            get_avatar_url, get_display_name, get_profile, set_avatar_url, set_display_name,
        },
        uiaa::AuthData,
    },
    assign,
    events::{
        ignored_user_list::{IgnoredUser, IgnoredUserListEventContent},
        media_preview_config::{
            InviteAvatars, MediaPreviewConfigEventContent, MediaPreviews,
            UnstableMediaPreviewConfigEventContent,
        },
        push_rules::PushRulesEventContent,
        room::MediaSource,
        AnyGlobalAccountDataEventContent, GlobalAccountDataEvent, GlobalAccountDataEventContent,
        GlobalAccountDataEventType, StaticEventContent,
    },
    push::Ruleset,
    serde::Raw,
    thirdparty::Medium,
    ClientSecret, MxcUri, OwnedMxcUri, OwnedRoomId, OwnedUserId, RoomId, SessionId, UInt, UserId,
};
use serde::Deserialize;
use tracing::error;

use crate::{config::RequestConfig, Client, Error, Result};

/// A high-level API to manage the client owner's account.
///
/// All the methods on this struct send a request to the homeserver.
#[derive(Debug, Clone)]
pub struct Account {
    /// The underlying HTTP client.
    client: Client,
}

impl Account {
    /// The maximum number of visited room identifiers to keep in the state
    /// store.
    const VISITED_ROOMS_LIMIT: usize = 20;

    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Get the display name of the account.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// let user = "example";
    /// let client = Client::new(homeserver).await?;
    /// client.matrix_auth().login_username(user, "password").send().await?;
    ///
    /// if let Some(name) = client.account().get_display_name().await? {
    ///     println!("Logged in as user '{user}' with display name '{name}'");
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn get_display_name(&self) -> Result<Option<String>> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        let request = get_display_name::v3::Request::new(user_id.to_owned());
        let request_config = self.client.request_config().force_auth();
        let response = self.client.send(request).with_request_config(request_config).await?;
        Ok(response.displayname)
    }

    /// Set the display name of the account.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// let user = "example";
    /// let client = Client::new(homeserver).await?;
    /// client.matrix_auth().login_username(user, "password").send().await?;
    ///
    /// client.account().set_display_name(Some("Alice")).await?;
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn set_display_name(&self, name: Option<&str>) -> Result<()> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        let request =
            set_display_name::v3::Request::new(user_id.to_owned(), name.map(ToOwned::to_owned));
        self.client.send(request).await?;
        Ok(())
    }

    /// Get the MXC URI of the account's avatar, if set.
    ///
    /// This always sends a request to the server to retrieve this information.
    /// If successful, this fills the cache, and makes it so that
    /// [`Self::get_cached_avatar_url`] will always return something.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let user = "example";
    /// let client = Client::new(homeserver).await?;
    /// client.matrix_auth().login_username(user, "password").send().await?;
    ///
    /// if let Some(url) = client.account().get_avatar_url().await? {
    ///     println!("Your avatar's mxc url is {url}");
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn get_avatar_url(&self) -> Result<Option<OwnedMxcUri>> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        let request = get_avatar_url::v3::Request::new(user_id.to_owned());

        let config = Some(RequestConfig::new().force_auth());

        let response = self.client.send(request).with_request_config(config).await?;
        if let Some(url) = response.avatar_url.clone() {
            // If an avatar is found cache it.
            let _ = self
                .client
                .state_store()
                .set_kv_data(
                    StateStoreDataKey::UserAvatarUrl(user_id),
                    StateStoreDataValue::UserAvatarUrl(url),
                )
                .await;
        } else {
            // If there is no avatar the user has removed it and we uncache it.
            let _ = self
                .client
                .state_store()
                .remove_kv_data(StateStoreDataKey::UserAvatarUrl(user_id))
                .await;
        }
        Ok(response.avatar_url)
    }

    /// Get the URL of the account's avatar, if is stored in cache.
    pub async fn get_cached_avatar_url(&self) -> Result<Option<OwnedMxcUri>> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        let data = self
            .client
            .state_store()
            .get_kv_data(StateStoreDataKey::UserAvatarUrl(user_id))
            .await?;
        Ok(data.map(|v| v.into_user_avatar_url().expect("Session data is not a user avatar url")))
    }

    /// Set the MXC URI of the account's avatar.
    ///
    /// The avatar is unset if `url` is `None`.
    pub async fn set_avatar_url(&self, url: Option<&MxcUri>) -> Result<()> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        let request =
            set_avatar_url::v3::Request::new(user_id.to_owned(), url.map(ToOwned::to_owned));
        self.client.send(request).await?;
        Ok(())
    }

    /// Get the account's avatar, if set.
    ///
    /// Returns the avatar.
    ///
    /// If a thumbnail is requested no guarantee on the size of the image is
    /// given.
    ///
    /// # Arguments
    ///
    /// * `format` - The desired format of the avatar.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::room_id;
    /// # use matrix_sdk::media::MediaFormat;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let user = "example";
    /// let client = Client::new(homeserver).await?;
    /// client.matrix_auth().login_username(user, "password").send().await?;
    ///
    /// if let Some(avatar) = client.account().get_avatar(MediaFormat::File).await?
    /// {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn get_avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        if let Some(url) = self.get_avatar_url().await? {
            let request = MediaRequestParameters { source: MediaSource::Plain(url), format };
            Ok(Some(self.client.media().get_media_content(&request, true).await?))
        } else {
            Ok(None)
        }
    }

    /// Upload and set the account's avatar.
    ///
    /// This will upload the data produced by the reader to the homeserver's
    /// content repository, and set the user's avatar to the MXC URI for the
    /// uploaded file.
    ///
    /// This is a convenience method for calling [`Media::upload()`],
    /// followed by [`Account::set_avatar_url()`].
    ///
    /// Returns the MXC URI of the uploaded avatar.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::fs;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let image = fs::read("/home/example/selfie.jpg")?;
    ///
    /// client.account().upload_avatar(&mime::IMAGE_JPEG, image).await?;
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [`Media::upload()`]: crate::Media::upload
    pub async fn upload_avatar(&self, content_type: &Mime, data: Vec<u8>) -> Result<OwnedMxcUri> {
        let upload_response = self.client.media().upload(content_type, data, None).await?;
        self.set_avatar_url(Some(&upload_response.content_uri)).await?;
        Ok(upload_response.content_uri)
    }

    /// Get the profile of the account.
    ///
    /// Allows to get both the display name and avatar URL in a single call.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let profile = client.account().fetch_user_profile().await?;
    /// println!(
    ///     "You are '{:?}' with avatar '{:?}'",
    ///     profile.displayname, profile.avatar_url
    /// );
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn fetch_user_profile(&self) -> Result<get_profile::v3::Response> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        self.fetch_user_profile_of(user_id).await
    }

    /// Get the profile for a given user id
    ///
    /// # Arguments
    ///
    /// * `user_id` the matrix id this function downloads the profile for
    pub async fn fetch_user_profile_of(
        &self,
        user_id: &UserId,
    ) -> Result<get_profile::v3::Response> {
        let request = get_profile::v3::Request::new(user_id.to_owned());
        Ok(self
            .client
            .send(request)
            .with_request_config(RequestConfig::short_retry().force_auth())
            .await?)
    }

    /// Change the password of the account.
    ///
    /// # Arguments
    ///
    /// * `new_password` - The new password to set.
    ///
    /// * `auth_data` - This request uses the [User-Interactive Authentication
    ///   API][uiaa]. The first request needs to set this to `None` and will
    ///   always fail with an [`UiaaResponse`]. The response will contain
    ///   information for the interactive auth and the same request needs to be
    ///   made but this time with some `auth_data` provided.
    ///
    /// # Returns
    ///
    /// This method might return an [`ErrorKind::WeakPassword`] error if the new
    /// password is considered insecure by the homeserver, with details about
    /// the strength requirements in the error's message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{
    /// #     api::client::{
    /// #         account::change_password::v3::{Request as ChangePasswordRequest},
    /// #         uiaa::{AuthData, Dummy},
    /// #     },
    /// #     assign,
    /// # };
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// client.account().change_password(
    ///     "myverysecretpassword",
    ///     Some(AuthData::Dummy(Dummy::new())),
    /// ).await?;
    /// # anyhow::Ok(()) };
    /// ```
    /// [uiaa]: https://spec.matrix.org/v1.2/client-server-api/#user-interactive-authentication-api
    /// [`UiaaResponse`]: ruma::api::client::uiaa::UiaaResponse
    /// [`ErrorKind::WeakPassword`]: ruma::api::client::error::ErrorKind::WeakPassword
    pub async fn change_password(
        &self,
        new_password: &str,
        auth_data: Option<AuthData>,
    ) -> Result<change_password::v3::Response> {
        let request = assign!(change_password::v3::Request::new(new_password.to_owned()), {
            auth: auth_data,
        });
        Ok(self.client.send(request).await?)
    }

    /// Deactivate this account definitively.
    ///
    /// # Arguments
    ///
    /// * `id_server` - The identity server from which to unbind the user’s
    ///   [Third Party Identifiers][3pid].
    ///
    /// * `auth_data` - This request uses the [User-Interactive Authentication
    ///   API][uiaa]. The first request needs to set this to `None` and will
    ///   always fail with an [`UiaaResponse`]. The response will contain
    ///   information for the interactive auth and the same request needs to be
    ///   made but this time with some `auth_data` provided.
    ///
    /// * `erase` - Whether the user would like their content to be erased as
    ///   much as possible from the server.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{
    /// #     api::client::{
    /// #         account::change_password::v3::{Request as ChangePasswordRequest},
    /// #         uiaa::{AuthData, Dummy},
    /// #     },
    /// #     assign,
    /// # };
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let account = client.account();
    /// let response = account.deactivate(None, None, false).await;
    ///
    /// // Proceed with UIAA.
    /// # anyhow::Ok(()) };
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [uiaa]: https://spec.matrix.org/v1.2/client-server-api/#user-interactive-authentication-api
    /// [`UiaaResponse`]: ruma::api::client::uiaa::UiaaResponse
    pub async fn deactivate(
        &self,
        id_server: Option<&str>,
        auth_data: Option<AuthData>,
        erase_data: bool,
    ) -> Result<deactivate::v3::Response> {
        let request = assign!(deactivate::v3::Request::new(), {
            id_server: id_server.map(ToOwned::to_owned),
            auth: auth_data,
            erase: erase_data,
        });
        Ok(self.client.send(request).await?)
    }

    /// Get the registered [Third Party Identifiers][3pid] on the homeserver of
    /// the account.
    ///
    /// These 3PIDs may be used by the homeserver to authenticate the user
    /// during sensitive operations.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let threepids = client.account().get_3pids().await?.threepids;
    ///
    /// for threepid in threepids {
    ///     println!(
    ///         "Found 3PID '{}' of type '{}'",
    ///         threepid.address, threepid.medium
    ///     );
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    pub async fn get_3pids(&self) -> Result<get_3pids::v3::Response> {
        let request = get_3pids::v3::Request::new();
        Ok(self.client.send(request).await?)
    }

    /// Request a token to validate an email address as a [Third Party
    /// Identifier][3pid].
    ///
    /// This is the first step in registering an email address as 3PID. Next,
    /// call [`Account::add_3pid()`] with the same `client_secret` and the
    /// returned `sid`.
    ///
    /// # Arguments
    ///
    /// * `client_secret` - A client-generated secret string used to protect
    ///   this session.
    ///
    /// * `email` - The email address to validate.
    ///
    /// * `send_attempt` - The attempt number. This number needs to be
    ///   incremented if you want to request another token for the same
    ///   validation.
    ///
    /// # Returns
    ///
    /// * `sid` - The session ID to be used in following requests for this 3PID.
    ///
    /// * `submit_url` - If present, the user will submit the token to the
    ///   client, that must send it to this URL. If not, the client will not be
    ///   involved in the token submission.
    ///
    /// This method might return an [`ErrorKind::ThreepidInUse`] error if the
    /// email address is already registered for this account or another, or an
    /// [`ErrorKind::ThreepidDenied`] error if it is denied.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{ClientSecret, uint};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let account = client.account();
    /// # let secret = ClientSecret::parse("secret")?;
    /// let token_response = account
    ///     .request_3pid_email_token(&secret, "john@matrix.org", uint!(0))
    ///     .await?;
    ///
    /// // Wait for the user to confirm that the token was submitted or prompt
    /// // the user for the token and send it to submit_url.
    ///
    /// let uiaa_response =
    ///     account.add_3pid(&secret, &token_response.sid, None).await;
    ///
    /// // Proceed with UIAA.
    /// # anyhow::Ok(()) };
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [`ErrorKind::ThreepidInUse`]: ruma::api::client::error::ErrorKind::ThreepidInUse
    /// [`ErrorKind::ThreepidDenied`]: ruma::api::client::error::ErrorKind::ThreepidDenied
    pub async fn request_3pid_email_token(
        &self,
        client_secret: &ClientSecret,
        email: &str,
        send_attempt: UInt,
    ) -> Result<request_3pid_management_token_via_email::v3::Response> {
        let request = request_3pid_management_token_via_email::v3::Request::new(
            client_secret.to_owned(),
            email.to_owned(),
            send_attempt,
        );
        Ok(self.client.send(request).await?)
    }

    /// Request a token to validate a phone number as a [Third Party
    /// Identifier][3pid].
    ///
    /// This is the first step in registering a phone number as 3PID. Next,
    /// call [`Account::add_3pid()`] with the same `client_secret` and the
    /// returned `sid`.
    ///
    /// # Arguments
    ///
    /// * `client_secret` - A client-generated secret string used to protect
    ///   this session.
    ///
    /// * `country` - The two-letter uppercase ISO-3166-1 alpha-2 country code
    ///   that the number in phone_number should be parsed as if it were dialled
    ///   from.
    ///
    /// * `phone_number` - The phone number to validate.
    ///
    /// * `send_attempt` - The attempt number. This number needs to be
    ///   incremented if you want to request another token for the same
    ///   validation.
    ///
    /// # Returns
    ///
    /// * `sid` - The session ID to be used in following requests for this 3PID.
    ///
    /// * `submit_url` - If present, the user will submit the token to the
    ///   client, that must send it to this URL. If not, the client will not be
    ///   involved in the token submission.
    ///
    /// This method might return an [`ErrorKind::ThreepidInUse`] error if the
    /// phone number is already registered for this account or another, or an
    /// [`ErrorKind::ThreepidDenied`] error if it is denied.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{ClientSecret, uint};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let account = client.account();
    /// # let secret = ClientSecret::parse("secret")?;
    /// let token_response = account
    ///     .request_3pid_msisdn_token(&secret, "FR", "0123456789", uint!(0))
    ///     .await?;
    ///
    /// // Wait for the user to confirm that the token was submitted or prompt
    /// // the user for the token and send it to submit_url.
    ///
    /// let uiaa_response =
    ///     account.add_3pid(&secret, &token_response.sid, None).await;
    ///
    /// // Proceed with UIAA.
    /// # anyhow::Ok(()) };
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [`ErrorKind::ThreepidInUse`]: ruma::api::client::error::ErrorKind::ThreepidInUse
    /// [`ErrorKind::ThreepidDenied`]: ruma::api::client::error::ErrorKind::ThreepidDenied
    pub async fn request_3pid_msisdn_token(
        &self,
        client_secret: &ClientSecret,
        country: &str,
        phone_number: &str,
        send_attempt: UInt,
    ) -> Result<request_3pid_management_token_via_msisdn::v3::Response> {
        let request = request_3pid_management_token_via_msisdn::v3::Request::new(
            client_secret.to_owned(),
            country.to_owned(),
            phone_number.to_owned(),
            send_attempt,
        );
        Ok(self.client.send(request).await?)
    }

    /// Add a [Third Party Identifier][3pid] on the homeserver for this
    /// account.
    ///
    /// This 3PID may be used by the homeserver to authenticate the user
    /// during sensitive operations.
    ///
    /// This method should be called after
    /// [`Account::request_3pid_email_token()`] or
    /// [`Account::request_3pid_msisdn_token()`] to complete the 3PID
    ///
    /// # Arguments
    ///
    /// * `client_secret` - The same client secret used in
    ///   [`Account::request_3pid_email_token()`] or
    ///   [`Account::request_3pid_msisdn_token()`].
    ///
    /// * `sid` - The session ID returned in
    ///   [`Account::request_3pid_email_token()`] or
    ///   [`Account::request_3pid_msisdn_token()`].
    ///
    /// * `auth_data` - This request uses the [User-Interactive Authentication
    ///   API][uiaa]. The first request needs to set this to `None` and will
    ///   always fail with an [`UiaaResponse`]. The response will contain
    ///   information for the interactive auth and the same request needs to be
    ///   made but this time with some `auth_data` provided.
    ///
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [uiaa]: https://spec.matrix.org/v1.2/client-server-api/#user-interactive-authentication-api
    /// [`UiaaResponse`]: ruma::api::client::uiaa::UiaaResponse
    pub async fn add_3pid(
        &self,
        client_secret: &ClientSecret,
        sid: &SessionId,
        auth_data: Option<AuthData>,
    ) -> Result<add_3pid::v3::Response> {
        #[rustfmt::skip] // rustfmt wants to merge the next two lines
        let request =
            assign!(add_3pid::v3::Request::new(client_secret.to_owned(), sid.to_owned()), {
                auth: auth_data
            });
        Ok(self.client.send(request).await?)
    }

    /// Delete a [Third Party Identifier][3pid] from the homeserver for this
    /// account.
    ///
    /// # Arguments
    ///
    /// * `address` - The 3PID being removed.
    ///
    /// * `medium` - The type of the 3PID.
    ///
    /// * `id_server` - The identity server to unbind from. If not provided, the
    ///   homeserver should unbind the 3PID from the identity server it was
    ///   bound to previously.
    ///
    /// # Returns
    ///
    /// * [`ThirdPartyIdRemovalStatus::Success`] if the 3PID was also unbound
    ///   from the identity server.
    ///
    /// * [`ThirdPartyIdRemovalStatus::NoSupport`] if the 3PID was not unbound
    ///   from the identity server. This can also mean that the 3PID was not
    ///   bound to an identity server in the first place.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::thirdparty::Medium;
    /// # use matrix_sdk::ruma::api::client::account::ThirdPartyIdRemovalStatus;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let account = client.account();
    /// match account
    ///     .delete_3pid("paul@matrix.org", Medium::Email, None)
    ///     .await?
    ///     .id_server_unbind_result
    /// {
    ///     ThirdPartyIdRemovalStatus::Success => {
    ///         println!("3PID unbound from the Identity Server");
    ///     }
    ///     _ => println!("Could not unbind 3PID from the Identity Server"),
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [`ThirdPartyIdRemovalStatus::Success`]: ruma::api::client::account::ThirdPartyIdRemovalStatus::Success
    /// [`ThirdPartyIdRemovalStatus::NoSupport`]: ruma::api::client::account::ThirdPartyIdRemovalStatus::NoSupport
    pub async fn delete_3pid(
        &self,
        address: &str,
        medium: Medium,
        id_server: Option<&str>,
    ) -> Result<delete_3pid::v3::Response> {
        let request = assign!(delete_3pid::v3::Request::new(medium, address.to_owned()), {
            id_server: id_server.map(ToOwned::to_owned),
        });
        Ok(self.client.send(request).await?)
    }

    /// Get the content of an account data event of statically-known type, from
    /// storage.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # async {
    /// # let client = Client::new("http://localhost:8080".parse()?).await?;
    /// # let account = client.account();
    /// use matrix_sdk::ruma::events::ignored_user_list::IgnoredUserListEventContent;
    ///
    /// let maybe_content = account.account_data::<IgnoredUserListEventContent>().await?;
    /// if let Some(raw_content) = maybe_content {
    ///     let content = raw_content.deserialize()?;
    ///     println!("Ignored users:");
    ///     for user_id in content.ignored_users.keys() {
    ///         println!("- {user_id}");
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn account_data<C>(&self) -> Result<Option<Raw<C>>>
    where
        C: GlobalAccountDataEventContent + StaticEventContent<IsPrefix = ruma::events::False>,
    {
        get_raw_content(self.client.state_store().get_account_data_event_static::<C>().await?)
    }

    /// Get the content of an account data event of a given type, from storage.
    pub async fn account_data_raw(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEventContent>>> {
        get_raw_content(self.client.state_store().get_account_data_event(event_type).await?)
    }

    /// Fetch a global account data event from the server.
    ///
    /// The content from the response will not be persisted in the store.
    ///
    /// Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # async {
    /// # let client = Client::new("http://localhost:8080".parse()?).await?;
    /// # let account = client.account();
    /// use matrix_sdk::ruma::events::{ignored_user_list::IgnoredUserListEventContent, GlobalAccountDataEventType};
    ///
    /// if let Some(raw_content) = account.fetch_account_data(GlobalAccountDataEventType::IgnoredUserList).await? {
    ///     let content = raw_content.deserialize_as_unchecked::<IgnoredUserListEventContent>()?;
    ///
    ///     println!("Ignored users:");
    ///
    ///     for user_id in content.ignored_users.keys() {
    ///         println!("- {user_id}");
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    pub async fn fetch_account_data(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEventContent>>> {
        let own_user = self.client.user_id().ok_or(Error::AuthenticationRequired)?;

        let request = get_global_account_data::v3::Request::new(own_user.to_owned(), event_type);

        match self.client.send(request).await {
            Ok(r) => Ok(Some(r.account_data)),
            Err(e) => {
                if let Some(kind) = e.client_api_error_kind() {
                    if kind == &ErrorKind::NotFound {
                        Ok(None)
                    } else {
                        Err(e.into())
                    }
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Fetch an account data event of statically-known type from the server.
    pub async fn fetch_account_data_static<C>(&self) -> Result<Option<Raw<C>>>
    where
        C: GlobalAccountDataEventContent + StaticEventContent<IsPrefix = ruma::events::False>,
    {
        Ok(self.fetch_account_data(C::TYPE.into()).await?.map(Raw::cast_unchecked))
    }

    /// Set the given account data event.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # async {
    /// # let client = Client::new("http://localhost:8080".parse()?).await?;
    /// # let account = client.account();
    /// use matrix_sdk::ruma::{
    ///     events::ignored_user_list::{IgnoredUser, IgnoredUserListEventContent},
    ///     user_id,
    /// };
    ///
    /// let mut content = account
    ///     .account_data::<IgnoredUserListEventContent>()
    ///     .await?
    ///     .map(|c| c.deserialize())
    ///     .transpose()?
    ///     .unwrap_or_default();
    /// content
    ///     .ignored_users
    ///     .insert(user_id!("@foo:bar.com").to_owned(), IgnoredUser::new());
    /// account.set_account_data(content).await?;
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn set_account_data<T>(
        &self,
        content: T,
    ) -> Result<set_global_account_data::v3::Response>
    where
        T: GlobalAccountDataEventContent,
    {
        let own_user = self.client.user_id().ok_or(Error::AuthenticationRequired)?;

        let request = set_global_account_data::v3::Request::new(own_user.to_owned(), &content)?;

        Ok(self.client.send(request).await?)
    }

    /// Set the given raw account data event.
    pub async fn set_account_data_raw(
        &self,
        event_type: GlobalAccountDataEventType,
        content: Raw<AnyGlobalAccountDataEventContent>,
    ) -> Result<set_global_account_data::v3::Response> {
        let own_user = self.client.user_id().ok_or(Error::AuthenticationRequired)?;

        let request =
            set_global_account_data::v3::Request::new_raw(own_user.to_owned(), event_type, content);

        Ok(self.client.send(request).await?)
    }

    /// Marks the room identified by `room_id` as a "direct chat" with each
    /// user in `user_ids`.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room ID of the direct message room.
    /// * `user_ids` - The user IDs to be associated with this direct message
    ///   room.
    pub async fn mark_as_dm(&self, room_id: &RoomId, user_ids: &[OwnedUserId]) -> Result<()> {
        use ruma::events::direct::DirectEventContent;

        // This function does a read/update/store of an account data event stored on the
        // homeserver. We first fetch the existing account data event, the event
        // contains a map which gets updated by this method, finally we upload the
        // modified event.
        //
        // To prevent multiple calls to this method trying to update the map of DMs same
        // time, and thus trampling on each other we introduce a lock which acts
        // as a semaphore.
        let _guard = self.client.locks().mark_as_dm_lock.lock().await;

        // Now we need to mark the room as a DM for ourselves, we fetch the
        // existing `m.direct` event and append the room to the list of DMs we
        // have with this user.

        // We are fetching the content from the server because we currently can't rely
        // on `/sync` giving us the correct data in a timely manner.
        let raw_content = self.fetch_account_data_static::<DirectEventContent>().await?;

        let mut content = if let Some(raw_content) = raw_content {
            // Log the error and pass it upwards if we fail to deserialize the m.direct
            // event.
            raw_content.deserialize().map_err(|err| {
                error!("unable to deserialize m.direct event content; aborting request to mark {room_id} as dm: {err}");
                err
            })?
        } else {
            // If there was no m.direct event server-side, create a default one.
            Default::default()
        };

        for user_id in user_ids {
            content.entry(user_id.into()).or_default().push(room_id.to_owned());
        }

        // TODO: We should probably save the fact that we need to send this out
        // because otherwise we might end up in a state where we have a DM that
        // isn't marked as one.
        self.set_account_data(content).await?;

        Ok(())
    }

    /// Adds the given user ID to the account's ignore list.
    pub async fn ignore_user(&self, user_id: &UserId) -> Result<()> {
        let own_user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        if user_id == own_user_id {
            return Err(Error::CantIgnoreLoggedInUser);
        }

        let mut ignored_user_list = self.get_ignored_user_list_event_content().await?;
        ignored_user_list.ignored_users.insert(user_id.to_owned(), IgnoredUser::new());

        self.set_account_data(ignored_user_list).await?;

        // In theory, we should also clear some caches here, because they may include
        // events sent by the ignored user. In practice, we expect callers to
        // take care of this, or subsystems to listen to user list changes and
        // clear caches accordingly.

        Ok(())
    }

    /// Removes the given user ID from the account's ignore list.
    pub async fn unignore_user(&self, user_id: &UserId) -> Result<()> {
        let mut ignored_user_list = self.get_ignored_user_list_event_content().await?;

        // Only update account data if the user was ignored in the first place.
        if ignored_user_list.ignored_users.remove(user_id).is_some() {
            self.set_account_data(ignored_user_list).await?;
        }

        // See comment in `ignore_user`.
        Ok(())
    }

    async fn get_ignored_user_list_event_content(&self) -> Result<IgnoredUserListEventContent> {
        let ignored_user_list = self
            .account_data::<IgnoredUserListEventContent>()
            .await?
            .map(|c| c.deserialize())
            .transpose()?
            .unwrap_or_default();
        Ok(ignored_user_list)
    }

    /// Get the current push rules from storage.
    ///
    /// If no push rules event was found, or it fails to deserialize, a ruleset
    /// with the server-default push rules is returned.
    ///
    /// Panics if called when the client is not logged in.
    pub async fn push_rules(&self) -> Result<Ruleset> {
        Ok(self
            .account_data::<PushRulesEventContent>()
            .await?
            .and_then(|r| match r.deserialize() {
                Ok(r) => Some(r.global),
                Err(e) => {
                    error!("Push rules event failed to deserialize: {e}");
                    None
                }
            })
            .unwrap_or_else(|| {
                Ruleset::server_default(
                    self.client.user_id().expect("The client should be logged in"),
                )
            }))
    }

    /// Retrieves the user's recently visited room list
    pub async fn get_recently_visited_rooms(&self) -> Result<Vec<OwnedRoomId>> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        let data = self
            .client
            .state_store()
            .get_kv_data(StateStoreDataKey::RecentlyVisitedRooms(user_id))
            .await?;

        Ok(data
            .map(|v| {
                v.into_recently_visited_rooms()
                    .expect("Session data is not a list of recently visited rooms")
            })
            .unwrap_or_default())
    }

    /// Moves/inserts the given room to the front of the recently visited list
    pub async fn track_recently_visited_room(&self, room_id: OwnedRoomId) -> Result<(), Error> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;

        // Get the previously stored recently visited rooms
        let mut recently_visited_rooms = self.get_recently_visited_rooms().await?;

        // Remove all other occurrences of the new room_id
        recently_visited_rooms.retain(|r| r != &room_id);

        // And insert it as the most recent
        recently_visited_rooms.insert(0, room_id);

        // Cap the whole list to the VISITED_ROOMS_LIMIT
        recently_visited_rooms.truncate(Self::VISITED_ROOMS_LIMIT);

        let data = StateStoreDataValue::RecentlyVisitedRooms(recently_visited_rooms);
        self.client
            .state_store()
            .set_kv_data(StateStoreDataKey::RecentlyVisitedRooms(user_id), data)
            .await?;
        Ok(())
    }

    /// Observes the media preview configuration.
    ///
    /// This value is linked to the [MSC 4278](https://github.com/matrix-org/matrix-spec-proposals/pull/4278) which is still in an unstable state.
    ///
    /// This will return the initial value of the configuration and a stream
    /// that will yield new values as they are received.
    ///
    /// The initial value is the one that was stored in the account data
    /// when the client was started.
    /// and the following code is using a temporary solution until we know which
    /// Matrix version will support the stable type.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use futures_util::{pin_mut, StreamExt};
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::events::media_preview_config::MediaPreviews;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let account = client.account();
    ///
    /// let (initial_config, config_stream) =
    ///     account.observe_media_preview_config().await?;
    ///
    /// println!("Initial media preview config: {:?}", initial_config);
    ///
    /// pin_mut!(config_stream);
    /// while let Some(new_config) = config_stream.next().await {
    ///     println!("Updated media preview config: {:?}", new_config);
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn observe_media_preview_config(
        &self,
    ) -> Result<
        (
            Option<MediaPreviewConfigEventContent>,
            impl Stream<Item = MediaPreviewConfigEventContent>,
        ),
        Error,
    > {
        // We need to create two observers, one for the stable event and one for the
        // unstable and combine them into a single stream.
        let first_observer = self
            .client
            .observe_events::<GlobalAccountDataEvent<MediaPreviewConfigEventContent>, ()>();

        let stream = first_observer.subscribe().map(|event| event.0.content);

        let second_observer = self
            .client
            .observe_events::<GlobalAccountDataEvent<UnstableMediaPreviewConfigEventContent>, ()>();

        let second_stream = second_observer.subscribe().map(|event| event.0.content.0);

        let mut combined_stream = stream::select(stream, second_stream);

        let result_stream = async_stream::stream! {
            // The observers need to be alive for the individual streams to be alive, so let's now
            // create a stream that takes ownership of them.
            let _first_observer = first_observer;
            let _second_observer = second_observer;

            while let Some(item) = combined_stream.next().await {
                yield item
            }
        };

        // We need to get the initial value of the media preview config event
        // we do this after creating the observers to make sure that we don't
        // create a race condition
        let initial_value = self.get_media_preview_config_event_content().await?;

        Ok((initial_value, result_stream))
    }

    /// Fetch the media preview configuration event content from the server.
    ///
    /// Will check first for the stable event and then for the unstable one.
    pub async fn fetch_media_preview_config_event_content(
        &self,
    ) -> Result<Option<MediaPreviewConfigEventContent>> {
        // First we check if there is a value in the stable event
        let media_preview_config =
            self.fetch_account_data_static::<MediaPreviewConfigEventContent>().await?;

        let media_preview_config = if let Some(media_preview_config) = media_preview_config {
            Some(media_preview_config)
        } else {
            // If there is no value in the stable event, we check the unstable
            self.fetch_account_data_static::<UnstableMediaPreviewConfigEventContent>()
                .await?
                .map(Raw::cast)
        };

        // We deserialize the content of the event, if is not found we return the
        // default
        let media_preview_config = media_preview_config.and_then(|value| value.deserialize().ok());

        Ok(media_preview_config)
    }

    /// Get the media preview configuration event content stored in the cache.
    ///
    /// Will check first for the stable event and then for the unstable one.
    pub async fn get_media_preview_config_event_content(
        &self,
    ) -> Result<Option<MediaPreviewConfigEventContent>> {
        let media_preview_config = self
            .account_data::<MediaPreviewConfigEventContent>()
            .await?
            .and_then(|r| r.deserialize().ok());

        if let Some(media_preview_config) = media_preview_config {
            Ok(Some(media_preview_config))
        } else {
            Ok(self
                .account_data::<UnstableMediaPreviewConfigEventContent>()
                .await?
                .and_then(|r| r.deserialize().ok())
                .map(Into::into))
        }
    }

    /// Set the media previews display policy in the timeline.
    ///
    /// This will always use the unstable event until we know which Matrix
    /// version will support it.
    pub async fn set_media_previews_display_policy(&self, policy: MediaPreviews) -> Result<()> {
        let mut media_preview_config =
            self.fetch_media_preview_config_event_content().await?.unwrap_or_default();
        media_preview_config.media_previews = Some(policy);

        // Updating the unstable account data
        let unstable_media_preview_config =
            UnstableMediaPreviewConfigEventContent::from(media_preview_config);
        self.set_account_data(unstable_media_preview_config).await?;
        Ok(())
    }

    /// Set the display policy for avatars in invite requests.
    ///
    /// This will always use the unstable event until we know which matrix
    /// version will support it.
    pub async fn set_invite_avatars_display_policy(&self, policy: InviteAvatars) -> Result<()> {
        let mut media_preview_config =
            self.fetch_media_preview_config_event_content().await?.unwrap_or_default();
        media_preview_config.invite_avatars = Some(policy);

        // Updating the unstable account data
        let unstable_media_preview_config =
            UnstableMediaPreviewConfigEventContent::from(media_preview_config);
        self.set_account_data(unstable_media_preview_config).await?;
        Ok(())
    }
}

fn get_raw_content<Ev, C>(raw: Option<Raw<Ev>>) -> Result<Option<Raw<C>>> {
    #[derive(Deserialize)]
    #[serde(bound = "C: Sized")] // Replace default Deserialize bound
    struct GetRawContent<C> {
        content: Raw<C>,
    }

    Ok(raw
        .map(|event| event.deserialize_as_unchecked::<GetRawContent<C>>())
        .transpose()?
        .map(|get_raw| get_raw.content))
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;

    use crate::{test_utils::client::MockClientBuilder, Error};

    #[async_test]
    async fn test_dont_ignore_oneself() {
        let client = MockClientBuilder::new(None).build().await;

        // It's forbidden to ignore the logged-in user.
        assert_matches!(
            client.account().ignore_user(client.user_id().unwrap()).await,
            Err(Error::CantIgnoreLoggedInUser)
        );
    }
}
