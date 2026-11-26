//! Amazon Bedrock: which hosts are Bedrock's, what each signs as, and the
//! signer they sign with. Bedrock speaks the two wire families this runtime
//! already encodes, so the only thing it needs of its own is
//! authentication: AWS Signature Version 4 and the AWS credential chain,
//! shared with its clients in [`agent_client::aws`].
//!
//! Requests stream their body from the store and never hold it. Mantle takes
//! the payload as `UNSIGNED-PAYLOAD` over TLS; Runtime refuses that, so its
//! bodies are read once to digest them and again as they are sent.
use crate::{Error, Result, tools::Credentials};
pub use agent_client::aws::{
    Aws, DEFAULT_REGION, Keys, Start, UNSIGNED_PAYLOAD, clock_skewed, payload, region,
};

impl From<agent_client::aws::Error> for Error {
    fn from(error: agent_client::aws::Error) -> Self {
        match error.detail {
            Some(detail) => Error::with(&error.code, detail),
            None => Error::new(&error.code),
        }
    }
}

/// The runtime's redaction: every key the signer installs stays out of the
/// tools' output, and the names it reads stay out of their shells.
impl agent_client::aws::Redact for Credentials {
    fn set(&self, name: &str, value: &str) {
        Credentials::set(self, name, value);
    }
    fn withhold(&self, name: &str) {
        Credentials::withhold(self, name);
    }
}

/// The region and signing name of a Bedrock endpoint, from its host:
/// `bedrock-runtime.{region}.amazonaws.com` signs as `bedrock`,
/// `bedrock-mantle.{region}.api.aws` as `bedrock-mantle`. Anything else is
/// not Bedrock.
pub fn endpoint(url: &reqwest::Url) -> Option<(String, &'static str)> {
    let host = url.host_str()?;
    let (service, region) = match host.strip_prefix("bedrock-runtime.") {
        Some(rest) => ("bedrock", rest.strip_suffix(".amazonaws.com")?),
        None => (
            "bedrock-mantle",
            host.strip_prefix("bedrock-mantle.")?
                .strip_suffix(".api.aws")?,
        ),
    };
    (!region.is_empty()
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then(|| (region.to_owned(), service))
}

/// Credentials for the Bedrock endpoint at `url`, resolved once now. Keys
/// the AWS CLI cannot resolve yet (no login, an expired SSO session) do not
/// stop the daemon: the binding is returned with the reason, and its first
/// call resolves again, so `aws sso login` fixes it in place.
pub async fn open(
    url: &reqwest::Url,
    redaction: Option<Credentials>,
    start: &Start,
) -> Result<(Aws, Option<Error>)> {
    let (region, service) = endpoint(url).ok_or(Error::with(
        "invalid_provider_url",
        "a signed provider's host is bedrock-runtime.REGION.amazonaws.com or bedrock-mantle.REGION.api.aws",
    ))?;
    let redaction = redaction.map(|credentials| Box::new(credentials) as Box<_>);
    let (aws, unresolved) = Aws::open(region, service, redaction, start).await;
    Ok((aws, unresolved.map(Error::from)))
}

/// Runtime computes the body's digest into the canonical request whatever
/// `x-amz-content-sha256` says; Mantle accepts `UNSIGNED-PAYLOAD`.
pub fn signs_payload(aws: &Aws) -> bool {
    aws.service() == "bedrock"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_hosts_name_their_region_and_signing_name() {
        let url = |u: &str| reqwest::Url::parse(u).unwrap();
        assert_eq!(
            endpoint(&url(
                "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1"
            )),
            Some(("us-east-1".into(), "bedrock-mantle"))
        );
        assert_eq!(
            endpoint(&url(
                "https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1"
            )),
            Some(("eu-west-1".into(), "bedrock"))
        );
        for other in [
            "https://api.anthropic.com/v1",
            "https://bedrock-runtime.amazonaws.com/x",
            "https://bedrock-mantle.us-east-1.example.com/x",
            "https://bedrock-runtime.US-EAST-1.evil.test.amazonaws.com.attacker/x",
        ] {
            assert_eq!(endpoint(&url(other)), None, "{other}");
        }
    }

    /// Runtime signs its bodies' digests and Mantle sends them unsigned;
    /// whole requests sign as botocore signs them (agent_client::aws).
    #[test]
    fn runtime_signs_its_payload_and_mantle_does_not() {
        let keys = || Keys::new("AKIDEXAMPLE".into(), "secret".into(), None);
        for (url, signs) in [
            (
                "https://bedrock-runtime.us-east-1.amazonaws.com/anthropic/v1/messages",
                true,
            ),
            (
                "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages",
                false,
            ),
        ] {
            let (region, service) = endpoint(&reqwest::Url::parse(url).unwrap()).unwrap();
            assert_eq!(
                signs_payload(&Aws::fixed(&region, service, keys())),
                signs,
                "{url}"
            );
        }
    }

    /// The key id, secret and token are all redacted from tool output, and
    /// the environment names that carried them are kept out of shells.
    #[test]
    fn every_part_of_the_keys_stays_out_of_tools() {
        use agent_client::aws::Redact;
        let credentials = Credentials::default();
        let redaction: &dyn Redact = &credentials;
        redaction.set("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
        redaction.set("AWS_SECRET_ACCESS_KEY", "s3cr3t");
        redaction.set("AWS_SESSION_TOKEN", "t0k3n");
        redaction.withhold("AWS_PROFILE");
        assert_eq!(
            credentials.names(),
            [
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
                "AWS_PROFILE"
            ]
        );
        let text = credentials.redact("AKIDEXAMPLE s3cr3t t0k3n".into());
        for value in ["AKIDEXAMPLE", "s3cr3t", "t0k3n"] {
            assert!(!text.contains(value), "{text}");
        }
    }

    /// A refusal to resolve keeps its code and the CLI's reason.
    #[test]
    fn the_signer_s_errors_keep_their_code_and_reason() {
        let error = Error::from(agent_client::aws::Error::with(
            "provider_aws_credentials_unavailable",
            "Token has expired",
        ));
        assert_eq!(
            (error.code.as_str(), error.detail.as_deref()),
            (
                "provider_aws_credentials_unavailable",
                Some("Token has expired")
            )
        );
        let error = Error::from(agent_client::aws::Error::new(
            "provider_aws_credentials_expired",
        ));
        assert_eq!(
            (error.code.as_str(), error.detail),
            ("provider_aws_credentials_expired", None)
        );
    }
}
