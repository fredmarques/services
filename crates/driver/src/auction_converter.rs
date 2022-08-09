use anyhow::{Context, Result};
use contracts::WETH9;
use gas_estimation::GasPriceEstimating;
use model::auction::Auction as AuctionModel;
use primitive_types::H160;
use solver::{
    liquidity::order_converter::OrderConverter, settlement::external_prices::ExternalPrices,
    solver::Auction,
};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

// TODO eventually this has to be part of the auction coming from the autopilot.
/// Determines how much time a solver has to compute solutions for an incoming `Auction`.
const RUN_DURATION: Duration = Duration::from_secs(15);

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait AuctionConverting: Send + Sync {
    async fn convert_auction(&self, model: AuctionModel) -> Result<Auction>;
}

pub struct AuctionConverter {
    pub order_converter: OrderConverter,
    pub gas_price_estimator: Arc<dyn GasPriceEstimating>,
    pub native_token: H160,
    pub run: AtomicU64,
}

impl AuctionConverter {
    pub fn new(
        native_token: WETH9,
        gas_price_estimator: Arc<dyn GasPriceEstimating>,
        fee_objective_scaling_factor: f64,
    ) -> Self {
        Self {
            order_converter: OrderConverter {
                native_token: native_token.clone(),
                fee_objective_scaling_factor,
            },
            gas_price_estimator,
            native_token: native_token.address(),
            run: AtomicU64::default(),
        }
    }
}

#[async_trait::async_trait]
impl AuctionConverting for AuctionConverter {
    async fn convert_auction(&self, auction: AuctionModel) -> Result<Auction> {
        let run = self.run.fetch_add(1, Ordering::SeqCst);
        let orders = auction
            .orders
            .into_iter()
            .filter_map(
                |order| match self.order_converter.normalize_limit_order(order) {
                    Ok(order) => Some(order),
                    Err(err) => {
                        // This should never happen unless we are getting malformed
                        // orders from the API - so raise an alert if this happens.
                        tracing::error!(?err, "error normalizing limit order");
                        None
                    }
                },
            )
            .collect::<Vec<_>>();
        anyhow::ensure!(
            orders.iter().any(|o| !o.is_liquidity_order),
            "auction contains no user orders"
        );

        tracing::info!(?orders, "got {} orders", orders.len());

        let external_prices =
            ExternalPrices::try_from_auction_prices(self.native_token, auction.prices)
                .context("malformed acution prices")?;
        tracing::debug!(?external_prices, "estimated prices");

        let gas_price = self
            .gas_price_estimator
            .estimate()
            .await
            .context("failed to estimate gas price")?;
        tracing::debug!("solving with gas price of {:?}", gas_price);

        Ok(Auction {
            id: auction.next_solver_competition,
            run,
            orders,
            liquidity: vec![],
            gas_price: gas_price.effective_gas_price(),
            deadline: Instant::now() + RUN_DURATION,
            external_prices,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gas_estimation::GasPrice1559;
    use maplit::btreemap;
    use model::order::{Order, OrderData, OrderMetadata, BUY_ETH_ADDRESS};
    use num::BigRational;
    use primitive_types::U256;
    use shared::dummy_contract;
    use shared::gas_price_estimation::FakeGasPriceEstimator;

    #[tokio::test]
    async fn converts_auction() {
        let token = H160::from_low_u64_be;
        let order = |sell_token, buy_token, with_error| Order {
            data: OrderData {
                sell_token: token(sell_token),
                buy_token: token(buy_token),
                buy_amount: 10.into(),
                sell_amount: 10.into(),
                partially_fillable: true,
                ..Default::default()
            },
            metadata: OrderMetadata {
                full_fee_amount: 100.into(),
                executed_buy_amount: if with_error { 100u8 } else { 1u8 }.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let gas_price = GasPrice1559 {
            base_fee_per_gas: 0.0,
            max_fee_per_gas: 10000.0,
            max_priority_fee_per_gas: 10000.0,
        };
        let gas_estimator = Arc::new(FakeGasPriceEstimator::new(gas_price));
        let native_token = dummy_contract!(WETH9, token(1));
        let converter = AuctionConverter::new(native_token.clone(), gas_estimator, 2.);
        let mut model = AuctionModel {
            block: 1,
            latest_settlement_block: 2,
            next_solver_competition: 3,
            orders: vec![order(1, 2, false), order(2, 3, false), order(1, 3, true)],
            prices: btreemap! { token(2) => U256::exp10(18), token(3) => U256::exp10(18) },
        };

        let auction = converter.convert_auction(model.clone()).await.unwrap();
        assert_eq!(auction.id, 3);
        assert_eq!(
            auction
                .deadline
                .duration_since(Instant::now())
                .as_secs_f64()
                .ceil(),
            RUN_DURATION.as_secs_f64()
        );
        assert_eq!(auction.run, 0);
        // only orders which don't have a logical error
        assert_eq!(auction.orders.len(), 2);
        assert_eq!(auction.orders[0].sell_token, token(1));
        assert_eq!(auction.orders[0].buy_token, token(2));
        assert_eq!(auction.orders[1].sell_token, token(2));
        assert_eq!(auction.orders[1].buy_token, token(3));

        // 100 total fee of 10% filled order with fee factor of 2.0 == 180 scaled fee
        assert_eq!(auction.orders[0].scaled_unsubsidized_fee, 180.into());
        assert_eq!(auction.orders[1].scaled_unsubsidized_fee, 180.into());
        assert!(auction.liquidity.is_empty());
        for t in &[native_token.address(), BUY_ETH_ADDRESS, token(2), token(3)] {
            assert_eq!(
                auction.external_prices.price(t).unwrap(),
                &BigRational::from_float(1.).unwrap()
            );
        }

        let auction = converter.convert_auction(model.clone()).await.unwrap();
        assert_eq!(auction.run, 1);

        // auction has to include at least 1 user order
        model.orders = vec![order(1, 2, false)];
        model.orders[0].metadata.is_liquidity_order = true;
        assert!(converter.convert_auction(model).await.is_err());
    }
}