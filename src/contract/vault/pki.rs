use std::{
    cmp::Ordering,
    time::{SystemTime, UNIX_EPOCH},
};

use openssl::{asn1::Asn1Time, x509::X509};
use serde_json::{Value, json};

use crate::{common::errors::MegaError, contract::vault::integration::vault_core::VaultCore};

// FIXME: A more official and robust ROLE name
const ROLE: &str = "test-role";

#[allow(unused)]
impl VaultCore {
    /// Initialize the Vault CA
    async fn init_ca(&self) -> Result<(), MegaError> {
        // err = not found
        if self.read_api("pki/ca/tls/pem").await.is_err() {
            self.config_ca().await?;
            self.generate_root(false).await?;
            self.config_role(json!({
                "ttl": "60d",
                "max_ttl": "365d",
                "key_type": "rsa",
                "key_bits": 4096,
                "country": "CN",
                "province": "Beijing",
                "locality": "Beijing",
                "organization": "OpenAtom-Mega",
                "no_store": false,
            }))
            .await?;
        }
        Ok(())
    }

    async fn config_ca(&self) -> Result<(), MegaError> {
        // mount pki backend to path: pki/
        let mount_data = json!({
            "type": "pki",
        })
        .as_object()
        .ok_or_else(|| MegaError::Other("PKI mount data must be a JSON object".to_string()))?
        .clone();

        self.write_api("sys/mounts/pki/", Some(mount_data)).await?;
        Ok(())
    }

    /// generate root cert, so that you can read from `pki/ca/tls/pem`
    /// - if `exported` is true, then the response will contain `private key`
    async fn generate_root(&self, exported: bool) -> Result<(), MegaError> {
        let key_type = "rsa";
        let key_bits = 4096;
        let common_name = "mega-ca";
        let req_data = json!({
            "common_name": common_name,
            "ttl": "365d",
            "country": "cn",
            "key_type": key_type,
            "key_bits": key_bits,
        })
        .as_object()
        .ok_or_else(|| MegaError::Other("PKI root request must be a JSON object".to_string()))?
        .clone();

        self.write_api(
            format!(
                "pki/root/tls/generate/{}",
                if exported { "exported" } else { "internal" }
            )
            .as_str(),
            Some(req_data),
        )
        .await?;
        Ok(())
    }

    /// - `data`: see `libvault::modules::pki::path_roles`'s `RoleEntry`
    ///  - This function configures a role for issuing certificates.
    ///  - The `ROLE` constant is used as the role name.
    ///
    ///  # Arguments
    ///  - `data`: A JSON object containing the role configuration data.
    ///
    ///  # Example
    ///   ```json
    ///   {
    ///     "ttl": "60d",
    ///     "max_ttl": "365d",
    ///     "key_type": "rsa",
    ///     "key_bits": 4096,
    ///     "country": "CN",
    ///     "province": "Beijing",
    ///     "locality": "Beijing",
    ///     "organization": "Open Atom-Mega",
    ///     "no_store": false
    ///   }
    ///   ```
    pub async fn config_role(&self, data: Value) -> Result<(), MegaError> {
        let role_data = data
            .as_object()
            .ok_or_else(|| MegaError::Other("PKI role data must be a JSON object".to_string()))?
            .clone();

        // config role
        self.write_api(format!("pki/roles/tls/{ROLE}"), Some(role_data))
            .await?;
        Ok(())
    }

    /// issue certificate
    /// - `data`: see `libvault::modules::pki::path_issue`
    /// - return: `(cert_pem, private_key)`
    pub async fn issue_cert(&self, data: Value) -> Result<(String, String), MegaError> {
        // let dns_sans = ["test.com", "a.test.com", "b.test.com"];
        let issue_data = data
            .as_object()
            .ok_or_else(|| MegaError::Other("PKI issue data must be a JSON object".to_string()))?
            .clone();

        // issue cert
        let resp_body = self
            .write_api(format!("pki/issue/tls/{ROLE}"), Some(issue_data))
            .await?;
        let cert_data = resp_body
            .and_then(|response| response.data)
            .ok_or_else(|| MegaError::Other("PKI issue response is missing data".to_string()))?;
        let certificate = cert_data
            .get("certificate")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                MegaError::Other("PKI issue response is missing certificate".to_string())
            })?
            .to_owned();
        let private_key = cert_data
            .get("private_key")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                MegaError::Other("PKI issue response is missing private_key".to_string())
            })?
            .to_owned();

        Ok((certificate, private_key))
    }

    /// Verify certificate: time & signature
    /// # Arguments
    /// - `cert_pem`: The PEM-encoded certificate to verify.
    ///
    /// # Returns
    /// - `true` if the certificate is valid, `false` otherwise.
    pub async fn verify_cert(&self, cert_pem: &[u8]) -> Result<bool, MegaError> {
        let root_cert = self.get_root_cert().await?;
        let ca_cert = X509::from_pem(root_cert.as_bytes())
            .map_err(|e| MegaError::Other(format!("failed to parse PKI root certificate: {e}")))?;

        let cert = X509::from_pem(cert_pem)
            .map_err(|e| MegaError::Other(format!("failed to parse certificate: {e}")))?;
        // verify time
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| MegaError::Other(format!("system time before UNIX epoch: {e}")))?
            .as_secs() as i64;
        let now = Asn1Time::from_unix(now)
            .map_err(|e| MegaError::Other(format!("failed to construct ASN.1 time: {e}")))?;
        let not_before = cert.not_before();
        let not_after = cert.not_after();
        match now.compare(not_before) {
            Ok(Ordering::Less) | Err(_) => return Ok(false),
            _ => {}
        }
        match now.compare(not_after) {
            Ok(Ordering::Greater) | Err(_) => return Ok(false),
            _ => {}
        }

        // verify signature
        let public_key = ca_cert
            .public_key()
            .map_err(|e| MegaError::Other(format!("failed to read PKI root public key: {e}")))?;
        cert.verify(&public_key)
            .map_err(|e| MegaError::Other(format!("failed to verify certificate: {e}")))
    }

    /// Get root certificate of CA
    pub async fn get_root_cert(&self) -> Result<String, MegaError> {
        let resp_ca_pem = self.read_api("pki/ca/tls/pem").await?.ok_or_else(|| {
            MegaError::Other("PKI root certificate response is empty".to_string())
        })?;
        let ca_data = resp_ca_pem.data.ok_or_else(|| {
            MegaError::Other("PKI root certificate response is missing data".to_string())
        })?;

        ca_data
            .get("certificate")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                MegaError::Other("PKI root certificate response is missing certificate".to_string())
            })
    }
}

#[allow(clippy::await_holding_lock)]
#[cfg(test)]
mod tests_raw {
    use std::{
        fs,
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    use libvault::logical::Response;
    use openssl::{asn1::Asn1Time, ec::EcKey, nid::Nid, pkey::PKey, rsa::Rsa, x509::X509};
    use serde_json::{Map, Value, json};

    use crate::{
        common::errors::VaultResult, contract::vault::integration::vault_core::VaultCore,
        jupiter::tests::test_storage,
    };

    async fn test_read_api(
        core: &VaultCore,
        path: &str,
        is_ok: bool,
    ) -> VaultResult<Option<Response>> {
        let resp = core.read_api(path).await;
        assert_eq!(resp.is_ok(), is_ok);
        resp
    }

    async fn test_write_api(
        core: &VaultCore,
        path: &str,
        is_ok: bool,
        data: Option<Map<String, Value>>,
    ) -> VaultResult<Option<Response>> {
        let resp = core.write_api(path, data).await;
        assert_eq!(resp.is_ok(), is_ok, "{path}: {resp:?}");
        resp
    }

    async fn test_pki_config_ca(core: &VaultCore) {
        let mount_data = json!({
            "type": "pki",
        })
        .as_object()
        .unwrap()
        .clone();

        let resp = test_write_api(core, "sys/mounts/pki/", true, Some(mount_data)).await;
        assert!(resp.is_ok());
    }

    async fn test_pki_config_role(core: &VaultCore) {
        let role_data = json!({
            "ttl": "60d",
            "max_ttl": "365d",
            "key_type": "rsa",
            "key_bits": 4096,
            "country": "CN",
            "province": "Beijing",
            "locality": "Beijing",
            "organization": "OpenAtom",
            "no_store": false,
        })
        .as_object()
        .unwrap()
        .clone();

        // config role
        assert!(
            test_write_api(core, "pki/roles/tls/test", true, Some(role_data))
                .await
                .is_ok()
        );
        let resp = test_read_api(core, "pki/roles/tls/test", true).await;
        assert!(resp.as_ref().unwrap().is_some());
        let resp = resp.unwrap();
        assert!(resp.is_some());
        let data = resp.unwrap().data;
        assert!(data.is_some());
        let role_data = data.unwrap();
        println!("role_data: {role_data:?}");
        assert_eq!(role_data["ttl"].as_u64().unwrap(), 60 * 24 * 60 * 60);
        assert_eq!(role_data["max_ttl"].as_u64().unwrap(), 365 * 24 * 60 * 60);
        assert_eq!(role_data["not_before_duration"].as_u64().unwrap(), 30);
        assert_eq!(role_data["key_type"].as_str().unwrap(), "rsa");
        assert_eq!(role_data["key_bits"].as_u64().unwrap(), 4096);
        assert_eq!(role_data["country"].as_str().unwrap(), "CN");
        assert_eq!(role_data["province"].as_str().unwrap(), "Beijing");
        assert_eq!(role_data["locality"].as_str().unwrap(), "Beijing");
        assert_eq!(role_data["organization"].as_str().unwrap(), "OpenAtom");
        assert!(!role_data["no_store"].as_bool().unwrap());
    }

    async fn test_pki_generate_root(core: &VaultCore, exported: bool, is_ok: bool) {
        let key_type = "rsa";
        let key_bits = 4096;
        let common_name = "test-ca";
        let req_data = json!({
            "common_name": common_name,
            "ttl": "365d",
            "country": "cn",
            "key_type": key_type,
            "key_bits": key_bits,
        })
        .as_object()
        .unwrap()
        .clone();
        // println!("generate root req_data: {:?}, is_ok: {}", req_data, is_ok);
        let resp = test_write_api(
            core,
            format!(
                "pki/root/tls/generate/{}",
                if exported { "exported" } else { "internal" }
            )
            .as_str(),
            is_ok,
            Some(req_data),
        )
        .await;
        if !is_ok {
            return;
        }
        let resp_body = resp.unwrap();
        assert!(resp_body.is_some());
        let data = resp_body.unwrap().data;
        assert!(data.is_some());
        let key_data = data.unwrap();

        let resp_ca_pem = test_read_api(core, "pki/ca/tls/pem", true).await;
        let resp_ca_pem_cert_data = resp_ca_pem.unwrap().unwrap().data.unwrap();

        let ca_cert = X509::from_pem(
            resp_ca_pem_cert_data["certificate"]
                .as_str()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let subject = ca_cert.subject_name();
        let cn = subject.entries_by_nid(Nid::COMMONNAME).next().unwrap();
        assert_eq!(cn.data().as_slice(), common_name.as_bytes());

        let not_after = Asn1Time::days_from_now(365).unwrap();
        let ttl_diff = ca_cert.not_after().diff(&not_after);
        assert!(ttl_diff.is_ok());
        let ttl_diff = ttl_diff.unwrap();
        assert_eq!(ttl_diff.days, 0);

        if exported {
            assert!(key_data["private_key_type"].as_str().is_some());
            assert_eq!(key_data["private_key_type"].as_str().unwrap(), key_type);
            assert!(key_data["private_key"].as_str().is_some());
            let private_key_pem = key_data["private_key"].as_str().unwrap();
            match key_type {
                "rsa" => {
                    let rsa_key = Rsa::private_key_from_pem(private_key_pem.as_bytes());
                    assert!(rsa_key.is_ok());
                    assert_eq!(rsa_key.unwrap().size() * 8, key_bits);
                }
                "ec" => {
                    let ec_key = EcKey::private_key_from_pem(private_key_pem.as_bytes());
                    assert!(ec_key.is_ok());
                    assert_eq!(ec_key.unwrap().group().degree(), key_bits);
                }
                _ => {}
            }
        } else {
            assert!(key_data.get("private_key").is_none());
        }
    }

    async fn test_pki_issue_cert_by_generate_root(core: &VaultCore) {
        let dns_sans = ["test.com", "a.test.com", "b.test.com"];
        let issue_data = json!({
            "ttl": "10d",
            "common_name": "test.com",
            "alt_names": "a.test.com,b.test.com",
        })
        .as_object()
        .unwrap()
        .clone();

        // issue cert
        let resp = test_write_api(core, "pki/issue/tls/test", true, Some(issue_data)).await;
        assert!(resp.is_ok());
        let resp_body = resp.unwrap();
        assert!(resp_body.is_some());
        let data = resp_body.unwrap().data;
        assert!(data.is_some());
        let cert_data = data.unwrap();
        println!("issue cert result: {:?}", cert_data["certificate"]);

        let mut file = fs::File::create("/tmp/cert.crt").unwrap();
        file.write_all(cert_data["certificate"].as_str().unwrap().as_ref())
            .unwrap();

        let cert = X509::from_pem(cert_data["certificate"].as_str().unwrap().as_bytes()).unwrap();
        let alt_names = cert.subject_alt_names();
        assert!(alt_names.is_some());
        let alt_names = alt_names.unwrap();
        assert_eq!(alt_names.len(), dns_sans.len());
        for alt_name in alt_names {
            assert!(dns_sans.contains(&alt_name.dnsname().unwrap()));
        }
        assert_eq!(cert_data["private_key_type"].as_str().unwrap(), "rsa");
        let priv_key =
            PKey::private_key_from_pem(cert_data["private_key"].as_str().unwrap().as_bytes())
                .unwrap();
        assert_eq!(priv_key.bits(), 4096);
        assert!(priv_key.public_eq(&cert.public_key().unwrap()));
        let serial_number = cert.serial_number().to_bn().unwrap();
        let serial_number_hex = serial_number.to_hex_str().unwrap();
        assert_eq!(
            cert_data["serial_number"]
                .as_str()
                .unwrap()
                .replace(':', "")
                .to_lowercase()
                .as_str(),
            serial_number_hex.to_lowercase().as_str()
        );
        let expiration_time =
            Asn1Time::from_unix(cert_data["expiration"].as_i64().unwrap()).unwrap();
        let ttl_compare = cert.not_after().compare(&expiration_time);
        assert!(ttl_compare.is_ok());
        assert_eq!(ttl_compare.unwrap(), std::cmp::Ordering::Equal);
        let now_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expiration_ttl = cert_data["expiration"].as_u64().unwrap();
        let ttl = expiration_ttl - now_timestamp;
        let expect_ttl = 10 * 24 * 60 * 60;
        assert!(ttl <= expect_ttl);
        assert!((ttl + 10) > expect_ttl);

        let authority_key_id = cert.authority_key_id();
        assert!(authority_key_id.is_some());

        println!(
            "authority_key_id: {}",
            hex::encode(authority_key_id.unwrap().as_slice())
        );

        let resp_ca_pem = test_read_api(core, "pki/ca/tls/pem", true).await;
        let resp_ca_pem_cert_data = resp_ca_pem.unwrap().unwrap().data.unwrap();

        let ca_cert = X509::from_pem(
            resp_ca_pem_cert_data["certificate"]
                .as_str()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let subject = ca_cert.subject_name();
        let cn = subject.entries_by_nid(Nid::COMMONNAME).next().unwrap();
        assert_eq!(cn.data().as_slice(), "test-ca".as_bytes());
        println!(
            "ca subject_key_id: {}",
            hex::encode(ca_cert.subject_key_id().unwrap().as_slice())
        );
        assert_eq!(
            ca_cert.subject_key_id().unwrap().as_slice(),
            authority_key_id.unwrap().as_slice()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_pki_module() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_path = temp_dir.path().join("key.json");
        let storage = test_storage(temp_dir.path()).await;
        let vault_core = VaultCore::config(storage.vault_storage(), key_path)
            .await
            .expect("vault core should initialize");

        {
            println!("Initializing Vault CA...");
            test_pki_config_ca(&vault_core).await;
            println!("Vault CA initialized.");
            test_pki_generate_root(&vault_core, false, true).await;
            println!("Root CA generated.");
            test_pki_config_role(&vault_core).await;
            println!("Role configured.");
            test_pki_issue_cert_by_generate_root(&vault_core).await;
            println!("Certificate issued.");
        }
    }
}
