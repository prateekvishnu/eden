/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::{bail, Error};
use fbinit::FacebookInit;
use http::HeaderMap;
use permission_checker::{MononokeIdentity, MononokeIdentitySet};

pub const HEADER_CRYPTO_AUTH_TOKENS: &str = "x-auth-cats";

#[cfg(not(fbcode_build))]
pub fn try_get_cats_idents(
    fb: FacebookInit,
    _headers: &HeaderMap,
) -> Result<Option<MononokeIdentitySet>, Error> {
    Ok(None)
}

#[cfg(fbcode_build)]
pub fn try_get_cats_idents(
    fb: FacebookInit,
    headers: &HeaderMap,
) -> Result<Option<MononokeIdentitySet>, Error> {
    let cats = match headers.get(HEADER_CRYPTO_AUTH_TOKENS) {
        Some(cats) => cats,
        None => return Ok(None),
    };

    let s_cats = cats.to_str()?;
    let cat_list = cryptocat::deserialize_crypto_auth_tokens(s_cats)?;
    let svc_scm_ident = cryptocat::Identity {
        id_type: "SERVICE_IDENTITY".to_string(),
        id_data: "scm_service_identity".to_string(),
        ..Default::default()
    };

    cat_list
        .tokens
        .into_iter()
        .try_fold(MononokeIdentitySet::new(), |mut idents_acc, token| {
            let tdata = cryptocat::deserialize_crypto_auth_token_data(
                &token.serializedCryptoAuthTokenData[..],
            )?;
            let m_ident =
                MononokeIdentity::new(tdata.signerIdentity.id_type, tdata.signerIdentity.id_data)?;
            idents_acc.insert(m_ident);
            let res = cryptocat::verify_crypto_auth_token(fb, token, &svc_scm_ident, None)?;
            if res.code != cryptocat::CATVerificationCode::SUCCESS {
                bail!(
                    "verification of CATs not successful. status code: {:?}",
                    res.code
                );
            }
            Ok(idents_acc)
        })
        .map(Option::Some)
}
