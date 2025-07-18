// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    jwks::{
        dummy_provider::{
            request_handler::{EquivocatingServer, StaticContentServer},
            DummyHttpServer,
        },
        get_patched_jwks, update_jwk_consensus_config,
    },
    smoke_test_environment::SwarmBuilder,
};
use aptos_forge::{NodeExt, Swarm, SwarmExt};
use aptos_logger::{debug, info};
use aptos_types::{
    jwks::{
        jwk::JWK, secure_test_rsa_jwk, unsupported::UnsupportedJWK, AllProvidersJWKs, ProviderJWKs,
    },
    keyless::test_utils::get_sample_iss,
    on_chain_config::{JWKConsensusConfigV1, OIDCProvider, OnChainJWKConsensusConfig},
};
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;

/// The validators should do JWK consensus per issuer:
/// one problematic issuer should not block valid updates of other issuers.
#[tokio::test]
#[ignore]
async fn jwk_consensus_per_issuer() {
    let epoch_duration_secs = 30;

    let (swarm, mut cli, _faucet) = SwarmBuilder::new_local(4)
        .with_num_fullnodes(1)
        .with_aptos()
        .with_init_genesis_config(Arc::new(move |conf| {
            conf.epoch_duration_secs = epoch_duration_secs;
        }))
        .build_with_cli(0)
        .await;
    let client = swarm.validators().next().unwrap().rest_client();
    let root_idx = cli.add_account_with_address_to_cli(
        swarm.root_key(),
        swarm.chain_info().root_account().address(),
    );
    swarm
        .wait_for_all_nodes_to_catchup_to_epoch(2, Duration::from_secs(epoch_duration_secs * 2))
        .await
        .expect("Epoch 2 taking too long to arrive!");

    info!("Initially the provider set is empty. The JWK map should have the secure test jwk added via a patch at genesis.");

    sleep(Duration::from_secs(10)).await;
    let patched_jwks = get_patched_jwks(&client).await;
    debug!("patched_jwks={:?}", patched_jwks);
    assert!(patched_jwks.jwks.entries.len() == 1);

    info!("Adding some providers, one seriously equivocating, the other well behaving.");
    let (alice_config_server, alice_jwks_server, bob_config_server, bob_jwks_server) = tokio::join!(
        DummyHttpServer::spawn(),
        DummyHttpServer::spawn(),
        DummyHttpServer::spawn(),
        DummyHttpServer::spawn()
    );
    let alice_issuer_id = "https://alice.io";
    let bob_issuer_id = "https://bob.dev";
    alice_config_server.update_request_handler(Some(Arc::new(StaticContentServer::new_str(
        format!(
            r#"{{"issuer": "{}", "jwks_uri": "{}"}}"#,
            alice_issuer_id,
            alice_jwks_server.url()
        )
        .as_str(),
    ))));
    bob_config_server.update_request_handler(Some(Arc::new(StaticContentServer::new_str(
        format!(
            r#"{{"issuer": "{}", "jwks_uri": "{}"}}"#,
            bob_issuer_id,
            bob_jwks_server.url()
        )
        .as_str(),
    ))));

    alice_jwks_server.update_request_handler(Some(Arc::new(EquivocatingServer::new(
        r#"{"keys": ["ALICE_JWK_V1A"]}"#.as_bytes().to_vec(),
        r#"{"keys": ["ALICE_JWK_V1B"]}"#.as_bytes().to_vec(),
        2,
    ))));
    bob_jwks_server.update_request_handler(Some(Arc::new(StaticContentServer::new(
        r#"{"keys": ["BOB_JWK_V0"]}"#.as_bytes().to_vec(),
    ))));
    let config = OnChainJWKConsensusConfig::V1(JWKConsensusConfigV1 {
        oidc_providers: vec![
            OIDCProvider {
                name: alice_issuer_id.to_string(),
                config_url: alice_config_server.url(),
            },
            OIDCProvider {
                name: bob_issuer_id.to_string(),
                config_url: bob_config_server.url(),
            },
        ],
    });

    let txn_summary = update_jwk_consensus_config(cli, root_idx, &config).await;
    debug!("txn_summary={:?}", txn_summary);

    info!("Wait for 60 secs and there should only update for Bob, not Alice.");
    sleep(Duration::from_secs(60)).await;
    let patched_jwks = get_patched_jwks(&client).await;
    debug!("patched_jwks={:?}", patched_jwks);
    assert_eq!(
        AllProvidersJWKs {
            entries: vec![
                ProviderJWKs {
                    issuer: bob_issuer_id.as_bytes().to_vec(),
                    version: 1,
                    jwks: vec![JWK::Unsupported(UnsupportedJWK::new_with_payload(
                        "\"BOB_JWK_V0\""
                    ))
                    .into()],
                },
                ProviderJWKs {
                    issuer: get_sample_iss().into_bytes(),
                    version: 0,
                    jwks: vec![secure_test_rsa_jwk().into()],
                },
            ]
        },
        patched_jwks.jwks
    );

    info!("Tear down.");
    tokio::join!(
        alice_jwks_server.shutdown(),
        alice_config_server.shutdown(),
        bob_jwks_server.shutdown(),
        bob_config_server.shutdown()
    );
}
