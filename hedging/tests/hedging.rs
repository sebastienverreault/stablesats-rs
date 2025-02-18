use futures::{stream::StreamExt, Stream};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serial_test::serial;

use std::{env, fs};

use okex_client::*;
use shared::{payload::*, pubsub::*};

use hedging::*;

#[derive(serde::Deserialize)]
struct Fixture {
    payloads: Vec<SynthUsdLiabilityPayload>,
}

fn load_fixture(path: &str) -> anyhow::Result<Fixture> {
    let contents = fs::read_to_string(path).expect("Couldn't load fixtures");
    Ok(serde_json::from_str(&contents)?)
}

fn okex_client_config() -> OkexClientConfig {
    let api_key = env::var("OKEX_API_KEY").expect("OKEX_API_KEY not set");
    let passphrase = env::var("OKEX_PASSPHRASE").expect("OKEX_PASS_PHRASE not set");
    let secret_key = env::var("OKEX_SECRET_KEY").expect("OKEX_SECRET_KEY not set");
    OkexClientConfig {
        api_key,
        passphrase,
        secret_key,
        simulated: true,
    }
}

async fn expect_exposure_between(
    mut stream: impl Stream<Item = Envelope<OkexBtcUsdSwapPositionPayload>> + Unpin,
    lower: Decimal,
    upper: Decimal,
) {
    let mut passed = false;
    for _ in 0..=10 {
        let pos = stream.next().await.unwrap().payload.signed_usd_exposure;
        passed = pos < upper && pos > lower;
        if passed {
            break;
        }
    }
    assert!(passed);
}

async fn expect_exposure_below(
    mut stream: impl Stream<Item = Envelope<OkexBtcUsdSwapPositionPayload>> + Unpin,
    expected: Decimal,
) {
    let mut passed = false;
    for _ in 0..=10 {
        let pos = stream.next().await.unwrap().payload.signed_usd_exposure;
        passed = pos < expected;
        if passed {
            break;
        }
    }
    assert!(passed);
}

async fn expect_exposure_equal(
    mut stream: impl Stream<Item = Envelope<OkexBtcUsdSwapPositionPayload>> + Unpin,
    expected: Decimal,
) {
    let mut passed = false;
    for _ in 0..=10 {
        let pos = stream.next().await.unwrap().payload.signed_usd_exposure;
        passed = pos == expected;
        if passed {
            break;
        }
    }
    assert!(passed);
}

#[tokio::test]
#[serial]
async fn hedging() -> anyhow::Result<()> {
    let redis_host = std::env::var("REDIS_HOST").unwrap_or("localhost".to_string());
    let pubsub_config = PubSubConfig {
        host: Some(redis_host),
        ..PubSubConfig::default()
    };
    let user_trades_pg_host = std::env::var("HEDGING_PG_HOST").unwrap_or("localhost".to_string());
    let user_trades_pg_port = std::env::var("HEDGING_PG_PORT").unwrap_or("5433".to_string());
    let pg_con = format!(
        "postgres://stablesats:stablesats@{user_trades_pg_host}:{user_trades_pg_port}/stablesats-hedging"
    );

    let publisher = Publisher::new(pubsub_config.clone()).await?;
    let subscriber = Subscriber::new(pubsub_config.clone()).await?;
    let mut stream = subscriber
        .subscribe::<OkexBtcUsdSwapPositionPayload>()
        .await?;

    tokio::spawn(async move {
        let (_, recv) = futures::channel::mpsc::unbounded();
        HedgingApp::run(
            recv,
            HedgingAppConfig {
                pg_con,
                migrate_on_start: true,
                okex_poll_frequency: std::time::Duration::from_secs(2),
            },
            okex_client_config(),
            pubsub_config,
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let mut payloads = load_fixture("./tests/fixtures/hedging.json")
        .expect("Couldn't load fixtures")
        .payloads
        .into_iter();
    publisher.publish(payloads.next().unwrap()).await?;

    let okex = OkexClient::new(okex_client_config()).await?;
    expect_exposure_equal(&mut stream, dec!(0)).await;

    publisher.publish(payloads.next().unwrap()).await?;

    for idx in 0..=1 {
        expect_exposure_between(&mut stream, dec!(-21000), dec!(-19000)).await;

        if idx == 0 {
            okex.place_order(
                ClientOrderId::new(),
                OkexOrderSide::Sell,
                &BtcUsdSwapContracts::from(5),
            )
            .await?;
            expect_exposure_below(&mut stream, dec!(-50000)).await;
        }
    }

    publisher.publish(payloads.next().unwrap()).await?;

    expect_exposure_equal(&mut stream, dec!(0)).await;

    Ok(())
}
