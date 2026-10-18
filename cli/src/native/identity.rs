//! Identity a page sees: client-hint metadata, language, timezone, locale and
//! screen.
//!
//! Chrome drops the `sec-ch-ua*` request headers and `navigator.userAgentData`
//! when a user agent is overridden without metadata, and its `Accept-Language`
//! header does not follow the locale override. These types carry the full set
//! from the `launch` command to every target the daemon creates, and
//! [`Emulation::apply`] sends them to a target before its first navigation.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::cdp::client::CdpClient;

/// One entry of a client-hint brand list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAgentBrand {
    pub brand: String,
    pub version: String,
}

/// `Emulation.UserAgentMetadata`, as given in the `userAgentMetadata` launch
/// field. Field names match the DevTools protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserAgentMetadata {
    /// Brands in `sec-ch-ua` and `navigator.userAgentData.brands`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brands: Option<Vec<UserAgentBrand>>,
    /// Brands with full versions, for `sec-ch-ua-full-version-list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_version_list: Option<Vec<UserAgentBrand>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_version: Option<String>,
    pub platform: String,
    #[serde(default)]
    pub platform_version: String,
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub mobile: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wow64: Option<bool>,
}

/// The `screen` launch field: what `screen.width`, `screen.height` and
/// `devicePixelRatio` report. The page's viewport keeps its own size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    #[serde(default = "Screen::default_scale")]
    pub device_scale_factor: f64,
}

impl Screen {
    fn default_scale() -> f64 {
        1.0
    }

    fn validate(&self) -> Result<(), String> {
        if self.width == 0 || self.height == 0 {
            return Err("'screen' width and height must be positive".to_string());
        }
        if !(self.device_scale_factor.is_finite() && self.device_scale_factor > 0.0) {
            return Err("'screen' deviceScaleFactor must be positive".to_string());
        }
        Ok(())
    }
}

/// The identity options of a `launch` command, other than the user agent
/// string itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Identity {
    pub user_agent_metadata: Option<UserAgentMetadata>,
    /// Languages for the `Accept-Language` request header, comma-separated
    /// without q-values (`ja-JP,ja`); Chrome adds the q-values itself.
    pub accept_language: Option<String>,
    /// IANA timezone ID.
    pub timezone: Option<String>,
    /// BCP 47 locale, e.g. `ja-JP`.
    pub locale: Option<String>,
    pub screen: Option<Screen>,
}

impl Identity {
    /// Read `userAgentMetadata`, `acceptLanguage`, `timezone`, `locale` and
    /// `screen` from a `launch` command. Absent fields stay unset; a field of
    /// the wrong shape is an error rather than silently ignored.
    pub fn from_launch_command(cmd: &Value) -> Result<Self, String> {
        let string_field = |key: &str| -> Result<Option<String>, String> {
            match cmd.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
                Some(_) => Err(format!("'{key}' must be a non-empty string")),
            }
        };
        let object_field = |key: &str| -> Result<Option<Value>, String> {
            match cmd.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(v) if v.is_object() => Ok(Some(v.clone())),
                Some(_) => Err(format!("'{key}' must be an object")),
            }
        };

        let user_agent_metadata = object_field("userAgentMetadata")?
            .map(serde_json::from_value::<UserAgentMetadata>)
            .transpose()
            .map_err(|e| format!("Invalid 'userAgentMetadata': {e}"))?;
        let screen = object_field("screen")?
            .map(serde_json::from_value::<Screen>)
            .transpose()
            .map_err(|e| format!("Invalid 'screen': {e}"))?;
        if let Some(screen) = &screen {
            screen.validate()?;
        }

        Ok(Self {
            user_agent_metadata,
            accept_language: string_field("acceptLanguage")?,
            timezone: string_field("timezone")?,
            locale: string_field("locale")?,
            screen,
        })
    }

    /// Stable text for the launch hash: a change relaunches the browser.
    pub fn fingerprint(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The identity to put on one target, borrowed from wherever it is kept.
#[derive(Debug, Default, Clone, Copy)]
pub struct Emulation<'a> {
    pub user_agent: Option<&'a str>,
    pub user_agent_metadata: Option<&'a UserAgentMetadata>,
    pub accept_language: Option<&'a str>,
    pub timezone: Option<&'a str>,
    pub locale: Option<&'a str>,
    pub screen: Option<&'a Screen>,
}

impl Emulation<'_> {
    /// Parameters of `Emulation.setUserAgentOverride`, or `None` when no part
    /// of the user agent, its metadata or the language is set. The protocol
    /// requires `userAgent`, so `current_user_agent` fills it in when only
    /// metadata or a language was given.
    pub fn user_agent_params(&self, current_user_agent: Option<&str>) -> Option<Value> {
        if self.user_agent.is_none()
            && self.user_agent_metadata.is_none()
            && self.accept_language.is_none()
        {
            return None;
        }
        let mut params = json!({
            "userAgent": self.user_agent.or(current_user_agent).unwrap_or_default(),
        });
        if let Some(language) = self.accept_language {
            params["acceptLanguage"] = json!(language);
        }
        if let Some(metadata) = self.user_agent_metadata {
            params["userAgentMetadata"] = serde_json::to_value(metadata).unwrap_or_default();
        }
        Some(params)
    }

    /// Parameters of `Emulation.setDeviceMetricsOverride` that set the screen
    /// and pixel ratio while leaving the viewport size alone (0 by 0).
    pub fn screen_params(&self) -> Option<Value> {
        self.screen.map(|screen| {
            json!({
                "width": 0,
                "height": 0,
                "deviceScaleFactor": screen.device_scale_factor,
                "mobile": false,
                "screenWidth": screen.width,
                "screenHeight": screen.height,
            })
        })
    }

    /// Send the identity to one target session. Call before the target's
    /// first navigation so its first request and first script already see it.
    pub async fn apply(&self, client: &CdpClient, session_id: &str) -> Result<(), String> {
        let needs_current_ua = self.user_agent.is_none()
            && (self.user_agent_metadata.is_some() || self.accept_language.is_some());
        let current_user_agent = if needs_current_ua {
            let version = client
                .send_command("Browser.getVersion", None, None)
                .await?;
            version
                .get("userAgent")
                .and_then(Value::as_str)
                .map(String::from)
        } else {
            None
        };

        if let Some(params) = self.user_agent_params(current_user_agent.as_deref()) {
            client
                .send_command(
                    "Emulation.setUserAgentOverride",
                    Some(params),
                    Some(session_id),
                )
                .await
                .map_err(|e| format!("Emulation.setUserAgentOverride failed: {e}"))?;
        }
        if let Some(timezone) = self.timezone {
            client
                .send_command(
                    "Emulation.setTimezoneOverride",
                    Some(json!({ "timezoneId": timezone })),
                    Some(session_id),
                )
                .await
                .map_err(|e| format!("Emulation.setTimezoneOverride failed: {e}"))?;
        }
        if let Some(locale) = self.locale {
            client
                .send_command(
                    "Emulation.setLocaleOverride",
                    Some(json!({ "locale": locale })),
                    Some(session_id),
                )
                .await
                .map_err(|e| format!("Emulation.setLocaleOverride failed: {e}"))?;
        }
        if let Some(params) = self.screen_params() {
            client
                .send_command(
                    "Emulation.setDeviceMetricsOverride",
                    Some(params),
                    Some(session_id),
                )
                .await
                .map_err(|e| format!("Emulation.setDeviceMetricsOverride failed: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata_json() -> Value {
        json!({
            "brands": [{ "brand": "Chromium", "version": "141" }],
            "fullVersionList": [{ "brand": "Chromium", "version": "141.0.1.2" }],
            "platform": "macOS",
            "platformVersion": "14.6.1",
            "architecture": "arm",
            "model": "",
            "mobile": false,
            "bitness": "64",
        })
    }

    #[test]
    fn launch_command_without_identity_fields_leaves_identity_unset() {
        let identity = Identity::from_launch_command(&json!({ "action": "launch" })).unwrap();
        assert_eq!(identity, Identity::default());
    }

    #[test]
    fn launch_command_identity_fields_are_read() {
        let identity = Identity::from_launch_command(&json!({
            "userAgentMetadata": metadata_json(),
            "acceptLanguage": "ja-JP,ja",
            "timezone": "Asia/Tokyo",
            "locale": "ja-JP",
            "screen": { "width": 1920, "height": 1080, "deviceScaleFactor": 2.0 },
        }))
        .unwrap();
        let metadata = identity.user_agent_metadata.unwrap();
        assert_eq!(metadata.platform, "macOS");
        assert_eq!(metadata.full_version_list.unwrap()[0].version, "141.0.1.2");
        assert_eq!(identity.accept_language.as_deref(), Some("ja-JP,ja"));
        assert_eq!(identity.timezone.as_deref(), Some("Asia/Tokyo"));
        assert_eq!(identity.locale.as_deref(), Some("ja-JP"));
        assert_eq!(
            identity.screen,
            Some(Screen {
                width: 1920,
                height: 1080,
                device_scale_factor: 2.0
            })
        );
    }

    #[test]
    fn screen_scale_defaults_to_one() {
        let identity =
            Identity::from_launch_command(&json!({ "screen": { "width": 800, "height": 600 } }))
                .unwrap();
        assert_eq!(identity.screen.unwrap().device_scale_factor, 1.0);
    }

    #[test]
    fn launch_command_identity_fields_of_the_wrong_shape_are_refused() {
        for (cmd, needle) in [
            (json!({ "timezone": 5 }), "'timezone'"),
            (json!({ "locale": "" }), "'locale'"),
            (json!({ "acceptLanguage": [] }), "'acceptLanguage'"),
            (json!({ "userAgentMetadata": "x" }), "'userAgentMetadata'"),
            (
                json!({ "userAgentMetadata": { "mobile": false } }),
                "Invalid 'userAgentMetadata'",
            ),
            (
                json!({ "userAgentMetadata": { "platform": "x", "brandz": [] } }),
                "Invalid 'userAgentMetadata'",
            ),
            (json!({ "screen": { "width": 0, "height": 1 } }), "positive"),
            (
                json!({ "screen": { "width": 1, "height": 1, "deviceScaleFactor": 0 } }),
                "positive",
            ),
            (json!({ "screen": { "width": 1 } }), "Invalid 'screen'"),
        ] {
            let error = Identity::from_launch_command(&cmd).unwrap_err();
            assert!(error.contains(needle), "{cmd}: {error}");
        }
    }

    #[test]
    fn user_agent_params_carry_metadata_and_language() {
        let metadata: UserAgentMetadata = serde_json::from_value(metadata_json()).unwrap();
        let params = Emulation {
            user_agent: Some("UA/1"),
            user_agent_metadata: Some(&metadata),
            accept_language: Some("ja-JP"),
            ..Emulation::default()
        }
        .user_agent_params(None)
        .unwrap();
        assert_eq!(params["userAgent"], "UA/1");
        assert_eq!(params["acceptLanguage"], "ja-JP");
        assert_eq!(params["userAgentMetadata"], metadata_json());
    }

    #[test]
    fn user_agent_params_without_a_user_agent_use_the_browsers_own() {
        let params = Emulation {
            accept_language: Some("de"),
            ..Emulation::default()
        }
        .user_agent_params(Some("Chrome/own"))
        .unwrap();
        assert_eq!(params["userAgent"], "Chrome/own");
    }

    #[test]
    fn nothing_is_sent_when_no_user_agent_part_is_set() {
        let emulation = Emulation {
            timezone: Some("UTC"),
            ..Emulation::default()
        };
        assert!(emulation.user_agent_params(Some("x")).is_none());
        assert!(emulation.screen_params().is_none());
    }

    #[test]
    fn screen_params_keep_the_viewport_size() {
        let screen = Screen {
            width: 1920,
            height: 1080,
            device_scale_factor: 2.0,
        };
        let params = Emulation {
            screen: Some(&screen),
            ..Emulation::default()
        }
        .screen_params()
        .unwrap();
        assert_eq!(params["width"], 0);
        assert_eq!(params["height"], 0);
        assert_eq!(params["screenWidth"], 1920);
        assert_eq!(params["screenHeight"], 1080);
        assert_eq!(params["deviceScaleFactor"], 2.0);
    }

    #[test]
    fn fingerprint_changes_with_any_identity_field() {
        let base = Identity::default();
        let timezone = Identity {
            timezone: Some("UTC".into()),
            ..Identity::default()
        };
        assert_ne!(base.fingerprint(), timezone.fingerprint());
    }
}
