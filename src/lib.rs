//#![deny(missing_docs)]
//#![deny(warnings)]

//! This library is useful for developing C/C++ AWS Nitro Enclave applications
//! with custom functionality like enclave-to-enclave
//! secure communication and mutual attestation.
//!
//!

use std::convert::TryFrom;

use aws_cose::crypto::Openssl;
use aws_nitro_enclaves_cose as aws_cose;
use aws_nitro_enclaves_nsm_api::api::{AttestationDoc, Digest};
use openssl::bn::BigNumContext;
use openssl::ec::*;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use serde_bytes::ByteBuf;
use x509_parser::prelude::*;

static ALL_SIGALGS: &[&webpki::SignatureAlgorithm] = &[
    &webpki::ECDSA_P256_SHA256,
    &webpki::ECDSA_P256_SHA384,
    &webpki::ECDSA_P384_SHA256,
    &webpki::ECDSA_P384_SHA384,
    &webpki::ED25519,
];

pub trait AttestationProcess {
    fn from_bytes(bytes: &[u8], root_cert: &[u8], unix_ts_sec: u64) -> anyhow::Result<Self>
    where
        Self: Sized;
}

pub const AWS_ROOT_CERT: &[u8] = include_bytes!("../tests/data/aws_root.der");

impl AttestationProcess for AttestationDoc {
    fn from_bytes(
        bytes: &[u8],
        root_cert: &[u8],
        unix_ts_sec: u64,
    ) -> anyhow::Result<AttestationDoc> {
        // for validation flow details see here:
        // https://github.com/aws/aws-nitro-enclaves-nsm-api/blob/main/docs/attestation_process.md
        let ad_doc_cose =
            aws_cose::CoseSign1::from_bytes(bytes).map_err(|err| anyhow::format_err!("{err}"))?;

        // no Signature checks for now - no key specified
        let ad_payload = ad_doc_cose
            .get_payload::<Openssl>(None)
            .map_err(|err| anyhow::format_err!("{err}"))?;
        // let ad_parsed: <AttestationDoc as AttestationProcess> = serde_cbor::from_slice(&ad_payload)?;
        let ad_parsed = AttestationDoc::from_binary(&ad_payload)
            .map_err(|err| anyhow::format_err!("{err:?}"))?;

        anyhow::ensure!(!ad_parsed.module_id.is_empty(), "module_id is empty");

        anyhow::ensure!(
            matches!(ad_parsed.digest, Digest::SHA384),
            "digest signature is unknown"
        );

        // validate timestamp range
        // let LocalResult::Single(ts_start) = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0) else {
        //     unreachable!()
        // };
        // let ts_end = Utc::now() + Duration::days(1);
        // anyhow::ensure!(
        //     ad_parsed.timestamp > ts_start && ad_parsed.timestamp < ts_end,
        //     "timestamp field has wrong value"
        // );

        // validate pcr map length
        let pcrs_len = ad_parsed.pcrs.len();
        anyhow::ensure!(
            (1..32).contains(&pcrs_len),
            "wrong number of PCRs in the map"
        );

        // validate pcr items
        for i in 0..pcrs_len {
            anyhow::ensure!(ad_parsed.pcrs.contains_key(&i), "PCR{i} is missing");

            let pcr_len = ad_parsed.pcrs[&i].len();
            anyhow::ensure!(
                [32, 48, 64].contains(&pcr_len),
                "PCR{i} len is other than 32/48/64 bytes"
            );
            //println!("prc{:2}:  {}", i, hex::encode( ad_parsed.pcrs[&i].to_vec() ) );
        }

        // validate 'certificate' member against
        // 'cabundle' with root cert replaced with our trusted hardcoded one
        let ee: &[u8] = &ad_parsed.certificate;

        let interm: Vec<ByteBuf> = ad_parsed.cabundle.clone();
        let interm = &interm[1..]; // skip first (claimed root) cert

        let interm_slices: Vec<_> = interm.iter().map(|x| x.as_slice()).collect();
        let interm_slices: &[&[u8]] = &interm_slices.to_vec();

        let anchors = vec![webpki::trust_anchor_util::cert_der_as_trust_anchor(root_cert).unwrap()];
        let anchors = webpki::TLSServerTrustAnchors(&anchors);

        let time = webpki::Time::from_seconds_since_unix_epoch(unix_ts_sec);

        let cert = webpki::EndEntityCert::from(ee)?;
        cert.verify_is_valid_tls_server_cert(ALL_SIGALGS, &anchors, interm_slices, time)?;

        let (rem, cert) = parse_x509_certificate(ee)?;
        anyhow::ensure!(rem.is_empty(), "rem is not empty");

        anyhow::ensure!(
            cert.tbs_certificate.version == X509Version::V3,
            "wrong cert version"
        );

        let ee_pub_key = cert.tbs_certificate.subject_pki.subject_public_key.data;

        let group = EcGroup::from_curve_name(Nid::SECP384R1).unwrap();
        let mut ctx = BigNumContext::new().unwrap();
        let point = EcPoint::from_bytes(&group, &ee_pub_key, &mut ctx).unwrap();
        let key = EcKey::from_public_key(&group, &point).unwrap();

        // [TODO] remove all above parse_x509_certificate() stuff and extract public key with webpki after issue
        // https://github.com/briansmith/webpki/issues/85
        // become fixed

        anyhow::ensure!(ad_doc_cose
            .verify_signature::<Openssl>(&PKey::try_from(key)?)
            .map_err(|err| anyhow::format_err!("{err}"))?);

        Ok(ad_parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_payload() -> anyhow::Result<()> {
        // current ee cert baked into the ../tests/data/nitro_ad_debug.bin attestation document has next time limits
        //
        // notBefore=Mar  5 17:01:49 2021 GMT
        // notAfter=Mar  5 20:01:49 2021 GMT
        //
        // let's substitute test timestamp within above range
        // Use next snippet to export cert
        //
        //let mut f = File::create("./_ee.der").expect("Could not run file!");
        //f.write_all(ee);
        //
        // Then, issue next cmd to see notBefore & notAfter from ./_ee.der
        // $openssl x509 -startdate -enddate -noout -inform der -in ./_ee.der

        let ad_blob = include_bytes!("../tests/data/nitro_ad_debug.bin");
        let root_cert = include_bytes!("../tests/data/aws_root.der");

        // Mar 5 18:00:00 2021 GMT
        <AttestationDoc as AttestationProcess>::from_bytes(ad_blob, root_cert, 1614967200)?;
        Ok(())
    }

    #[test]
    #[should_panic]
    fn test_broken_root_cert() {
        let ad_blob = include_bytes!("../tests/data/nitro_ad_debug.bin");
        let root_cert = include_bytes!("../tests/data/aws_root.der");
        let mut root_cert_copy = *root_cert;

        root_cert_copy[200] = 0xff;
        <AttestationDoc as AttestationProcess>::from_bytes(ad_blob, &root_cert_copy, 1614967200)
            .unwrap(); // Mar 5 18:00:00 2021 GMT
    }

    #[test]
    #[should_panic]
    fn test_expired_ee_cert() {
        let ad_blob = include_bytes!("../tests/data/nitro_ad_debug.bin");
        let root_cert = include_bytes!("../tests/data/aws_root.der");
        <AttestationDoc as AttestationProcess>::from_bytes(ad_blob, root_cert, 1618407754).unwrap();
    }

    #[test]
    #[should_panic]
    fn test_notyetvalid_ee_cert() {
        let ad_blob = include_bytes!("../tests/data/nitro_ad_debug.bin");
        let root_cert = include_bytes!("../tests/data/aws_root.der");
        <AttestationDoc as AttestationProcess>::from_bytes(ad_blob, root_cert, 1614947200).unwrap();
    }

    #[test]
    #[should_panic]
    fn test_broken_some_cert_in_ad() {
        let ad_blob = include_bytes!("../tests/data/nitro_ad_debug.bin");
        let root_cert = include_bytes!("../tests/data/aws_root.der");
        let mut ad_blob_copy = *ad_blob;

        ad_blob_copy[0x99f] = 0xff;
        <AttestationDoc as AttestationProcess>::from_bytes(&ad_blob_copy, root_cert, 1614967200)
            .unwrap();
    }

    #[test]
    #[should_panic]
    fn test_broken_ad_pcrx() {
        let ad_blob = include_bytes!("../tests/data/nitro_ad_debug.bin");
        let root_cert = include_bytes!("../tests/data/aws_root.der");
        let mut ad_blob_copy = *ad_blob;

        ad_blob_copy[0x13b] = 0xff;
        <AttestationDoc as AttestationProcess>::from_bytes(&ad_blob_copy, root_cert, 1614967200)
            .unwrap();
    }

    #[test]
    #[should_panic]
    fn test_broken_ad_debug_pcrx() {
        // mutate zero-filled PCR

        let ad_blob = include_bytes!("../tests/data/nitro_ad_debug.bin");
        let root_cert = include_bytes!("../tests/data/aws_root.der");
        let mut ad_blob_copy = *ad_blob;

        ad_blob_copy[0x281] = 0xff;
        <AttestationDoc as AttestationProcess>::from_bytes(&ad_blob_copy, root_cert, 1614967200)
            .unwrap();
    }

    #[test]
    fn cose_sign1_ec384_validate() -> anyhow::Result<()> {
        let (_, ec_public) = get_ec384_test_key();

        const TEXT: &[u8] = b"It is a truth universally acknowledged, that a single man in possession of a good fortune, must be in want of a wife.";

        // This output was validated against COSE-C implementation
        let cose_doc = aws_cose::CoseSign1::from_bytes(&[
            0x84, /* Protected: {1: -35} */
            0x44, 0xA1, 0x01, 0x38, 0x22, /* Unprotected: {4: '11'} */
            0xA1, 0x04, 0x42, 0x31, 0x31, /* payload: */
            0x58, 0x75, 0x49, 0x74, 0x20, 0x69, 0x73, 0x20, 0x61, 0x20, 0x74, 0x72, 0x75, 0x74,
            0x68, 0x20, 0x75, 0x6E, 0x69, 0x76, 0x65, 0x72, 0x73, 0x61, 0x6C, 0x6C, 0x79, 0x20,
            0x61, 0x63, 0x6B, 0x6E, 0x6F, 0x77, 0x6C, 0x65, 0x64, 0x67, 0x65, 0x64, 0x2C, 0x20,
            0x74, 0x68, 0x61, 0x74, 0x20, 0x61, 0x20, 0x73, 0x69, 0x6E, 0x67, 0x6C, 0x65, 0x20,
            0x6D, 0x61, 0x6E, 0x20, 0x69, 0x6E, 0x20, 0x70, 0x6F, 0x73, 0x73, 0x65, 0x73, 0x73,
            0x69, 0x6F, 0x6E, 0x20, 0x6F, 0x66, 0x20, 0x61, 0x20, 0x67, 0x6F, 0x6F, 0x64, 0x20,
            0x66, 0x6F, 0x72, 0x74, 0x75, 0x6E, 0x65, 0x2C, 0x20, 0x6D, 0x75, 0x73, 0x74, 0x20,
            0x62, 0x65, 0x20, 0x69, 0x6E, 0x20, 0x77, 0x61, 0x6E, 0x74, 0x20, 0x6F, 0x66, 0x20,
            0x61, 0x20, 0x77, 0x69, 0x66, 0x65, 0x2E, /* signature - length 48 x 2 */
            0x58, 0x60, /* R: */
            0xCD, 0x42, 0xD2, 0x76, 0x32, 0xD5, 0x41, 0x4E, 0x4B, 0x54, 0x5C, 0x95, 0xFD, 0xE6,
            0xE3, 0x50, 0x5B, 0x93, 0x58, 0x0F, 0x4B, 0x77, 0x31, 0xD1, 0x4A, 0x86, 0x52, 0x31,
            0x75, 0x26, 0x6C, 0xDE, 0xB2, 0x4A, 0xFF, 0x2D, 0xE3, 0x36, 0x4E, 0x9C, 0xEE, 0xE9,
            0xF9, 0xF7, 0x95, 0xA0, 0x15, 0x15, /* S: */
            0x5B, 0xC7, 0x12, 0xAA, 0x28, 0x63, 0xE2, 0xAA, 0xF6, 0x07, 0x8A, 0x81, 0x90, 0x93,
            0xFD, 0xFC, 0x70, 0x59, 0xA3, 0xF1, 0x46, 0x7F, 0x64, 0xEC, 0x7E, 0x22, 0x1F, 0xD1,
            0x63, 0xD8, 0x0B, 0x3B, 0x55, 0x26, 0x25, 0xCF, 0x37, 0x9D, 0x1C, 0xBB, 0x9E, 0x51,
            0x38, 0xCC, 0xD0, 0x7A, 0x19, 0x31,
        ])
        .map_err(|err| anyhow::format_err!("{err}"))?;

        let payload = cose_doc
            .get_payload::<Openssl>(Some(&PKey::try_from(ec_public)?))
            .map_err(|err| anyhow::format_err!("{err}"))?;
        anyhow::ensure!(payload == TEXT);
        Ok(())
    }

    #[test]
    fn aws_root_cert_used_as_end_entity_cert() {
        let ee: &[u8] = include_bytes!("../tests/data/aws_root.der");
        let ca = include_bytes!("../tests/data/aws_root.der");

        let anchors = vec![webpki::trust_anchor_util::cert_der_as_trust_anchor(ca).unwrap()];
        let anchors = webpki::TLSServerTrustAnchors(&anchors);

        let time = webpki::Time::from_seconds_since_unix_epoch(1616094379); // 18 March 2021

        let cert = webpki::EndEntityCert::from(ee).unwrap();
        assert_eq!(
            Err(webpki::Error::CAUsedAsEndEntity),
            cert.verify_is_valid_tls_server_cert(ALL_SIGALGS, &anchors, &[], time)
        );
    }

    ////////////////////////////////////////////////////////////////////////////////////////////////////////////////

    use aws_cose::crypto::Openssl;
    use openssl::pkey::{Private, Public};

    /// Static SECP384R1/P-384 key to be used when cross-validating the implementation
    fn get_ec384_test_key() -> (EcKey<Private>, EcKey<Public>) {
        let alg = openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::SECP384R1).unwrap();
        let x = openssl::bn::BigNum::from_hex_str(
            "5a829f62f2f4f095c0e922719285b4b981c677912870a413137a5d7319916fa8\
                584a6036951d06ffeae99ca73ab1a2dc",
        )
        .unwrap();
        let y = openssl::bn::BigNum::from_hex_str(
            "e1b76e08cb20d6afcea7423f8b49ec841dde6f210a6174750bf8136a31549422\
                4df153184557a6c29a1d7994804f604c",
        )
        .unwrap();
        let d = openssl::bn::BigNum::from_hex_str(
            "55c6aa815a31741bc37f0ffddea73af2397bad640816ef22bfb689efc1b6cc68\
                2a73f7e5a657248e3abad500e46d5afc",
        )
        .unwrap();
        let ec_public =
            openssl::ec::EcKey::from_public_key_affine_coordinates(&alg, &x, &y).unwrap();
        let ec_private =
            openssl::ec::EcKey::from_private_components(&alg, &d, ec_public.public_key()).unwrap();
        (
            //PKey::from_ec_key(ec_private).unwrap(),
            //PKey::from_ec_key(ec_public).unwrap(),
            ec_private, ec_public,
        )
    }
}

// cSpell:words chrono secp384r1
// cSpell:ignore cose webpki pkey cabundle interm
