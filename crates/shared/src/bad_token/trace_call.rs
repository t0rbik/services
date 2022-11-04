use super::{token_owner_finder::TokenOwnerFinding, BadTokenDetecting, TokenQuality};
use crate::{trace_many, Web3};
use anyhow::{anyhow, bail, ensure, Context, Result};
use contracts::ERC20;
use ethcontract::{dyns::DynTransport, transaction::TransactionBuilder, PrivateKey};
use primitive_types::{H160, U256};
use std::sync::Arc;
use thiserror::Error;
use web3::{
    signing::keccak256,
    types::{BlockTrace, CallRequest, Res},
};

/// Detects whether a token is "bad" (works in unexpected ways that are problematic for solving) by
/// simulating several transfers of a token. To find an initial address to transfer from we use
/// the amm pair providers.
/// Tokens are bad if:
/// - we cannot find an amm pool of the token to one of the base tokens
/// - transfer into the settlement contract or back out fails
/// - a transfer loses total balance
pub struct TraceCallDetector {
    pub web3: Web3,
    pub finder: Arc<dyn TokenOwnerFinding>,
    pub settlement_contract: H160,
}

#[async_trait::async_trait]
impl BadTokenDetecting for TraceCallDetector {
    async fn detect(&self, token: H160) -> Result<TokenQuality> {
        let quality = self.detect_with_retries(token).await?;
        tracing::debug!("token {:?} quality {:?}", token, quality);
        Ok(quality)
    }
}

impl TraceCallDetector {
    async fn detect_with_retries(&self, token: H160) -> Result<TokenQuality> {
        // It is possible, because of race conditions, for the token owner balance
        // to change between fetching it, and executing the trace call. Additionally,
        // block propagation delays may cause the trace_call to be exectued on an
        // earlier block than the one used for fetching token owner balances. As a
        // work-around, retry a few times.
        const MAX_RETRIES: usize = 3;
        for _ in 0..MAX_RETRIES {
            match self.detect_impl(token).await {
                Ok(quality) => return Ok(quality),
                Err(DetectError::BalanceChanged) => continue,
                Err(DetectError::Other(err)) => return Err(err),
            }
        }

        Err(DetectError::BalanceChanged.into())
    }

    async fn detect_impl(&self, token: H160) -> Result<TokenQuality, DetectError> {
        // Arbitrary amount that is large enough that small relative fees should be visible.
        const MIN_AMOUNT: u64 = 100_000;
        let (take_from, amount) = match self.finder.find_owner(token, MIN_AMOUNT.into()).await? {
            Some((address, balance)) => {
                tracing::debug!(
                    "testing token {:?} with pool {:?} amount {}",
                    token,
                    address,
                    balance
                );
                (address, balance)
            }
            None => return Ok(TokenQuality::bad("no pool")),
        };

        // We transfer the full available amount of the token from the amm pool into the
        // settlement contract and then to an arbitrary address.
        // Note that gas use can depend on the recipient because for the standard implementation
        // sending to an address that does not have any balance yet (implicitly 0) causes an
        // allocation.
        let request = self.create_trace_request(token, amount, take_from);
        let traces = trace_many::trace_many(request, &self.web3)
            .await
            .context("failed to trace for bad token detection")?;
        let response = Self::handle_response(&traces, amount)?;

        Ok(response)
    }

    // For the out transfer we use an arbitrary address without balance to detect tokens that
    // usually apply fees but not if the the sender or receiver is specifically exempt like
    // their own uniswap pools.
    fn arbitrary_recipient() -> H160 {
        PrivateKey::from_raw(keccak256(b"moo"))
            .unwrap()
            .public_address()
    }

    fn create_trace_request(&self, token: H160, amount: U256, take_from: H160) -> Vec<CallRequest> {
        let instance = ERC20::at(&self.web3, token);

        let mut requests = Vec::new();

        // 0
        let tx = instance.balance_of(take_from).m.tx;
        requests.push(call_request(None, token, tx));
        // 1
        let tx = instance.balance_of(self.settlement_contract).m.tx;
        requests.push(call_request(None, token, tx));
        // 2
        let tx = instance.transfer(self.settlement_contract, amount).tx;
        requests.push(call_request(Some(take_from), token, tx));
        // 3
        let tx = instance.balance_of(self.settlement_contract).m.tx;
        requests.push(call_request(None, token, tx));
        // 4
        let recipient = Self::arbitrary_recipient();
        let tx = instance.balance_of(recipient).m.tx;
        requests.push(call_request(None, token, tx));
        // 5
        let tx = instance.transfer(recipient, amount).tx;
        requests.push(call_request(Some(self.settlement_contract), token, tx));
        // 6
        let tx = instance.balance_of(self.settlement_contract).m.tx;
        requests.push(call_request(None, token, tx));
        // 7
        let tx = instance.balance_of(recipient).m.tx;
        requests.push(call_request(None, token, tx));

        // 8
        let tx = instance.approve(recipient, U256::MAX).tx;
        requests.push(call_request(Some(self.settlement_contract), token, tx));

        requests
    }

    fn handle_response(traces: &[BlockTrace], amount: U256) -> Result<TokenQuality, DetectError> {
        if traces.len() != 9 {
            return Err(anyhow!("unexpected number of traces").into());
        }

        match decode_u256(&traces[0]) {
            Ok(balance) if balance < amount => return Err(DetectError::BalanceChanged),
            Ok(_) => (),
            Err(_) => {
                return Ok(TokenQuality::bad(
                    "can't decode initial token owner balance",
                ))
            }
        }

        let gas_in = match ensure_transaction_ok_and_get_gas(&traces[2])? {
            Ok(gas) => gas,
            Err(reason) => {
                return Ok(TokenQuality::bad(format!(
                    "can't transfer into settlement contract: {reason}"
                )))
            }
        };
        let gas_out = match ensure_transaction_ok_and_get_gas(&traces[5])? {
            Ok(gas) => gas,
            Err(reason) => {
                return Ok(TokenQuality::bad(format!(
                    "can't transfer out of settlement contract: {reason}"
                )))
            }
        };

        let balance_before_in = match decode_u256(&traces[1]) {
            Ok(balance) => balance,
            Err(_) => return Ok(TokenQuality::bad("can't decode initial settlement balance")),
        };
        let balance_after_in = match decode_u256(&traces[3]) {
            Ok(balance) => balance,
            Err(_) => return Ok(TokenQuality::bad("can't decode middle settlement balance")),
        };
        let balance_after_out = match decode_u256(&traces[6]) {
            Ok(balance) => balance,
            Err(_) => return Ok(TokenQuality::bad("can't decode final settlement balance")),
        };

        let balance_recipient_before = match decode_u256(&traces[4]) {
            Ok(balance) => balance,
            Err(_) => return Ok(TokenQuality::bad("can't decode recipient balance before")),
        };

        let balance_recipient_after = match decode_u256(&traces[7]) {
            Ok(balance) => balance,
            Err(_) => return Ok(TokenQuality::bad("can't decode recipient balance after")),
        };

        tracing::debug!(%amount, %balance_before_in, %balance_after_in, %balance_after_out);

        // todo: Maybe do >= checks in case token transfer for whatever reason grants user more than
        // an amount transferred like an anti fee.

        let computed_balance_after_in = match balance_before_in.checked_add(amount) {
            Some(amount) => amount,
            None => {
                return Ok(TokenQuality::bad(
                    "token total supply does not fit a uint256",
                ))
            }
        };
        if balance_after_in != computed_balance_after_in {
            return Ok(TokenQuality::bad(
                "balance after in transfer does not match",
            ));
        }
        if balance_after_out != balance_before_in {
            return Ok(TokenQuality::bad(
                "balance after out transfer does not match",
            ));
        }
        let computed_balance_recipient_after = match balance_recipient_before.checked_add(amount) {
            Some(amount) => amount,
            None => {
                return Ok(TokenQuality::bad(
                    "token total supply does not fit a uint256",
                ))
            }
        };
        if computed_balance_recipient_after != balance_recipient_after {
            return Ok(TokenQuality::bad("balance of recipient does not match"));
        }

        if let Err(err) = ensure_transaction_ok_and_get_gas(&traces[8])? {
            return Ok(TokenQuality::bad(format!(
                "can't approve max amount: {}",
                err
            )));
        }

        let _gas_per_transfer = (gas_in + gas_out) / 2;
        Ok(TokenQuality::Good)
    }
}

fn call_request(
    from: Option<H160>,
    to: H160,
    transaction: TransactionBuilder<DynTransport>,
) -> CallRequest {
    let calldata = transaction.data.unwrap();
    CallRequest {
        from,
        to: Some(to),
        data: Some(calldata),
        ..Default::default()
    }
}

fn decode_u256(trace: &BlockTrace) -> Result<U256> {
    let bytes = trace.output.0.as_slice();
    ensure!(bytes.len() == 32, "invalid length");
    Ok(U256::from_big_endian(bytes))
}

// The outer result signals communication failure with the node.
// The inner result is Ok(gas_price) or Err if the transaction failed.
fn ensure_transaction_ok_and_get_gas(trace: &BlockTrace) -> Result<Result<U256, String>> {
    let transaction_traces = trace
        .trace
        .as_ref()
        .ok_or_else(|| anyhow!("trace not set"))?;
    let first = transaction_traces
        .first()
        .ok_or_else(|| anyhow!("expected at least one trace"))?;
    if let Some(error) = &first.error {
        return Ok(Err(format!("transaction failed: {error}")));
    }
    let call_result = match &first.result {
        Some(Res::Call(call)) => call,
        _ => bail!("no error but also no call result"),
    };
    Ok(Ok(call_result.gas_used))
}

// Trace call detection error.
#[derive(Debug, Error)]
enum DetectError {
    #[error("token owner balance changed")]
    BalanceChanged,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bad_token::token_owner_finder::{
            blockscout::BlockscoutTokenOwnerFinder,
            liquidity::{
                BalancerVaultFinder, FeeValues, UniswapLikePairProviderFinder, UniswapV3Finder,
            },
            TokenOwnerFinder,
        },
        sources::{sushiswap, uniswap_v2},
        transport::create_env_test_transport,
    };
    use contracts::{BalancerV2Vault, IUniswapV3Factory};
    use hex_literal::hex;
    use web3::types::{
        Action, ActionType, Bytes, Call, CallResult, CallType, Res, TransactionTrace,
    };

    fn encode_u256(u256: U256) -> Bytes {
        let mut bytes = vec![0u8; 32];
        u256.to_big_endian(&mut bytes);
        Bytes(bytes)
    }

    #[test]
    fn handle_response_ok() {
        let traces = &[
            BlockTrace {
                output: encode_u256(0.into()),
                trace: None,
                vm_trace: None,
                state_diff: None,
                transaction_hash: None,
            },
            BlockTrace {
                output: Default::default(),
                trace: Some(vec![TransactionTrace {
                    trace_address: Vec::new(),
                    subtraces: 0,
                    action: Action::Call(Call {
                        from: H160::zero(),
                        to: H160::zero(),
                        value: 0.into(),
                        gas: 0.into(),
                        input: Bytes(Vec::new()),
                        call_type: CallType::None,
                    }),
                    action_type: ActionType::Call,
                    result: Some(Res::Call(CallResult {
                        gas_used: 1.into(),
                        output: Bytes(Vec::new()),
                    })),
                    error: None,
                }]),
                vm_trace: None,
                state_diff: None,
                transaction_hash: None,
            },
            BlockTrace {
                output: encode_u256(1.into()),
                trace: None,
                vm_trace: None,
                state_diff: None,
                transaction_hash: None,
            },
            BlockTrace {
                output: encode_u256(0.into()),
                trace: None,
                vm_trace: None,
                state_diff: None,
                transaction_hash: None,
            },
            BlockTrace {
                output: Default::default(),
                trace: Some(vec![TransactionTrace {
                    trace_address: Vec::new(),
                    subtraces: 0,
                    action: Action::Call(Call {
                        from: H160::zero(),
                        to: H160::zero(),
                        value: 0.into(),
                        gas: 0.into(),
                        input: Bytes(Vec::new()),
                        call_type: CallType::None,
                    }),
                    action_type: ActionType::Call,
                    result: Some(Res::Call(CallResult {
                        gas_used: 3.into(),
                        output: Bytes(Vec::new()),
                    })),
                    error: None,
                }]),
                vm_trace: None,
                state_diff: None,
                transaction_hash: None,
            },
            BlockTrace {
                output: encode_u256(0.into()),
                trace: None,
                vm_trace: None,
                state_diff: None,
                transaction_hash: None,
            },
            BlockTrace {
                output: encode_u256(1.into()),
                trace: None,
                vm_trace: None,
                state_diff: None,
                transaction_hash: None,
            },
            BlockTrace {
                output: Default::default(),
                trace: Some(vec![TransactionTrace {
                    trace_address: Vec::new(),
                    subtraces: 0,
                    action: Action::Call(Call {
                        from: H160::zero(),
                        to: H160::zero(),
                        value: 0.into(),
                        gas: 0.into(),
                        input: Bytes(Vec::new()),
                        call_type: CallType::None,
                    }),
                    action_type: ActionType::Call,
                    result: Some(Res::Call(CallResult {
                        gas_used: 1.into(),
                        output: Bytes(Vec::new()),
                    })),
                    error: None,
                }]),
                vm_trace: None,
                state_diff: None,
                transaction_hash: None,
            },
        ];

        let result = TraceCallDetector::handle_response(traces, 1.into()).unwrap();
        let expected = TokenQuality::Good;
        assert_eq!(result, expected);
    }

    #[test]
    fn arbitrary_recipient_() {
        println!("{:?}", TraceCallDetector::arbitrary_recipient());
    }

    // cargo test -p shared mainnet_tokens -- --nocapture --ignored
    #[tokio::test]
    #[ignore]
    async fn mainnet_tokens() {
        // shared::tracing::initialize("orderbook::bad_token=debug,shared::transport=debug", tracing::level_filters::LevelFilter::OFF);
        let http = create_env_test_transport();
        let web3 = Web3::new(http);

        let base_tokens = &[
            testlib::tokens::WETH,
            testlib::tokens::DAI,
            testlib::tokens::USDC,
            testlib::tokens::USDT,
            testlib::tokens::COMP,
            testlib::tokens::MKR,
            testlib::tokens::WBTC,
        ];

        // tokens from our deny list
        let bad_tokens = &[
            addr!("0027449Bf0887ca3E431D263FFDeFb244D95b555"), // All balances are maxuint256
            addr!("0189d31f6629c359007f72b8d5ec8fa1c126f95c"),
            addr!("01995786f1435743c42b7f2276c496a610b58612"),
            addr!("072c46f392e729c1f0d92a307c2c6dba06b5d078"),
            addr!("074545177a36ab81aac783211f25e14f1ed03c2b"),
            addr!("07be1ead7aebee544618bdc688fa3cff09857c32"),
            addr!("0858a26055d6584e5b47bbecf7f7e8cbc390995b"),
            addr!("0aacfbec6a24756c20d41914f2caba817c0d8521"),
            addr!("0ba45a8b5d5575935b8158a88c631e9f9c95a2e5"),
            addr!("0e69d0a2bbb30abcb7e5cfea0e4fde19c00a8d47"),
            addr!("1016f3c0a1939fa27538339da7e2a300031b6f37"),
            addr!("106552c11272420aad5d7e94f8acab9095a6c952"),
            addr!("106d3c66d22d2dd0446df23d7f5960752994d600"),
            addr!("1337DEF18C680aF1f9f45cBcab6309562975b1dD"),
            addr!("1341a2257fa7b770420ef70616f888056f90926c"),
            addr!("1426cc6d52d1b14e2b3b1cb04d57ea42b39c4c7c"),
            addr!("14dd7ebe6cb084cb73ef377e115554d47dc9d61e"),
            addr!("15874d65e649880c2614e7a480cb7c9a55787ff6"),
            addr!("1681bcb589b3cfcf0c0616b0ce9b19b240643dc1"),
            addr!("18bdfc80b97cb97f6b466cce967849ce9cd9d58c"),
            addr!("1b9baf2a3edea91ee431f02d449a1044d5726669"),
            addr!("2129ff6000b95a973236020bcd2b2006b0d8e019"),
            addr!("239dc02a28a0774738463e06245544a72745d5c5"),
            addr!("251457b7c5d85251ca1ab384361c821330be2520"),
            addr!("25a1de1c3ee658fe034b8914a1d8d34110423af8"),
            addr!("26a79bd709a7ef5e5f747b8d8f83326ea044d8cc"),
            addr!("289d5488ab09f43471914e572ec9e3651c735af2"),
            addr!("298d492e8c1d909d3f63bc4a36c66c64acb3d695"),
            addr!("2b1fe2cea92436e8c34b7c215af66aaa2932a8b2"),
            addr!("31acf54fae6166dc2f90c4d6f20d379965e96bc1"),
            addr!("32c868f6318d6334b2250f323d914bc2239e4eee"),
            addr!("33f128394af03db639107473e52d84ff1290499e"),
            addr!("37611b28aca5673744161dc337128cfdd2657f69"),
            addr!("389999216860ab8e0175387a0c90e5c52522c945"),
            addr!("39b8523fa094b0dc045e2c3e5dff34b3f2ca6220"),
            addr!("3a6fe4c752eb8d571a660a776be4003d619c30a3"),
            addr!("3a9fff453d50d4ac52a6890647b823379ba36b9e"),
            addr!("3ea50b7ef6a7eaf7e966e2cb72b519c16557497c"),
            addr!("3fca773d13f831753ec3ae9f39ad4a6814ebb695"),
            addr!("41933422dc4a1cb8c822e06f12f7b52fa5e7e094"),
            addr!("45734927fa2f616fbe19e65f42a0ef3d37d1c80a"),
            addr!("45804880de22913dafe09f4980848ece6ecbaf78"),
            addr!("48be867b240d2ffaff69e0746130f2c027d8d3d2"),
            addr!("4a6be56a211a4c4e0dd4474d524138933c17f3e3"),
            addr!("4b86e0295e7d32433ffa6411b82b4f4e56a581e1"),
            addr!("4ba6ddd7b89ed838fed25d208d4f644106e34279"),
            addr!("4bae380b5d762d543d426331b8437926443ae9ec"),
            addr!("4bcddfcfa8cb923952bcf16644b36e5da5ca3184"),
            addr!("4c9d5672ae33522240532206ab45508116daf263"),
            addr!("4F9254C83EB525f9FCf346490bbb3ed28a81C667"),
            addr!("4fab740779c73aa3945a5cf6025bf1b0e7f6349c"),
            addr!("51d3e4c0b2c83e62f5d517d250b3e856897d2052"),
            addr!("53ba22cb4e5e9c1be0d73913764f572192a71aca"),
            addr!("56de8bc61346321d4f2211e3ac3c0a7f00db9b76"),
            addr!("576097fa17e1f702bb9167f0f08f2ea0898a3ea5"),
            addr!("577e7f9fa80ab33e87a01b701114257c8d9455a8"),
            addr!("586c680e9a6d21b81ebecf46d78844dab7b3bcf9"),
            addr!("5d0fa08aeb173ade44b0cf7f31d506d8e04f0ac8"),
            addr!("62359ed7505efc61ff1d56fef82158ccaffa23d7"),
            addr!("63d0eea1d7c0d1e89d7e665708d7e8997c0a9ed6"),
            addr!("66d31def9c47b62184d7f57175eed5b5d9b7f038"),
            addr!("671ab077497575dcafb68327d2d2329207323e74"),
            addr!("685aea4f02e39e5a5bb7f7117e88db1151f38364"),
            addr!("68e0a48d3bff6633a31d1d100b70f93c3859218b"),
            addr!("69692d3345010a207b759a7d1af6fc7f38b35c5e"),
            addr!("6a00b86e30167f73e38be086081b80213e8266aa"),
            addr!("6b8e77d3db1faa17f7b24c24242b6a1eb5008a16"),
            addr!("6e10aacb89a28d6fa0fe68790777fec7e7f01890"),
            addr!("6fcb6408499a7c0f242e32d77eb51ffa1dd28a7e"),
            addr!("714599f7604144a3fe1737c440a70fc0fd6503ea"),
            addr!("75fef397d74a2d11b64e6915cd847c1e7f8e5520"),
            addr!("76851a93977bea9264c32255b6457882035c7501"),
            addr!("79ba92dda26fce15e1e9af47d5cfdfd2a093e000"),
            addr!("7f0f118d083d5175ab9d2d34c4c8fa4f43c3f47b"),
            addr!("7ff4169a6b5122b664c51c95727d87750ec07c84"),
            addr!("801ea8c463a776e85344c565e355137b5c3324cd"),
            addr!("88ef27e69108b2633f8e1c184cc37940a075cc02"),
            addr!("8c7424c3000942e5a93de4a01ce2ec86c06333cb"),
            addr!("8eb24319393716668d768dcec29356ae9cffe285"),
            addr!("910524678c0b1b23ffb9285a81f99c29c11cbaed"),
            addr!("910985ffa7101bf5801dd2e91555c465efd9aab3"),
            addr!("925f2c11b99c1a4c46606898ee91ed3d450cfeda"),
            addr!("944eee930933be5e23b690c8589021ec8619a301"),
            addr!("94987bc8aa5f36cb2461c190134929a29c3df726"),
            addr!("97ad070879be5c31a03a1fe7e35dfb7d51d0eef1"),
            addr!("97b65710d03e12775189f0d113202cc1443b0aa2"),
            addr!("98ecf3d8e21adaafe16c00cc3ff681e72690278b"),
            addr!("99043bb680ab9262c7b2ac524e00b215efb7db9b"),
            addr!("99ddddd8dfe33905338a073047cfad72e6833c06"),
            addr!("9a514389172863f12854ad40090aa4b928028542"),
            addr!("9af15d7b8776fa296019979e70a5be53c714a7ec"),
            addr!("9ea3b5b4ec044b70375236a281986106457b20ef"),
            addr!("9f41da75ab2b8c6f0dcef7173c4bf66bd4f6b36a"),
            addr!("a03f1250aa448226ed4066d8d1722ddd8b51df59"),
            addr!("a2b4c0af19cc16a6cfacce81f192b024d625817d"),
            addr!("a3e059c0b01f07f211c85bf7b4f1d907afb011df"),
            addr!("a5959e9412d27041194c3c3bcbe855face2864f7"),
            addr!("a9a8377287ea9c6b8b4249dd502e75d34148fc5b"),
            addr!("adaa92cba08434c22d036c4115a6b3d7e2b5569b"),
            addr!("aee53701e18d5ff6af4964c3a381e7d09b9b9075"),
            addr!("b893a8049f250b57efa8c62d51527a22404d7c9a"),
            addr!("B96f0e9bb32760091eb2D6B0A5Ca0D2C7b5644B1"),
            addr!("ba7435a4b4c747e0101780073eeda872a69bdcd4"),
            addr!("bae5f2d8a1299e5c4963eaff3312399253f27ccb"),
            addr!("bd36b14c63f483b286c7b49b6eaffb2fe10aabc4"),
            addr!("bdea5bb640dbfc4593809deec5cdb8f99b704cd2"),
            addr!("bf04e48c5d8880306591ef888cde201d3984eb3e"),
            addr!("bf25ea982b4f850dafb4a95367b890eee5a9e8f2"),
            addr!("bf494f02ee3fde1f20bee6242bce2d1ed0c15e47"),
            addr!("c03841b5135600312707d39eb2af0d2ad5d51a91"),
            addr!("c10bbb8fd399d580b740ed31ff5ac94aa78ba9ed"),
            addr!("c12d1c73ee7dc3615ba4e37e4abfdbddfa38907e"),
            addr!("c40af1e4fecfa05ce6bab79dcd8b373d2e436c4e"),
            addr!("c4d586ef7be9ebe80bd5ee4fbd228fe2db5f2c4e"),
            addr!("c50ef449171a51fbeafd7c562b064b6471c36caa"),
            addr!("c626d951eff8e421448074bd2ad7805c6d585793"),
            addr!("c73c167e7a4ba109e4052f70d5466d0c312a344d"),
            addr!("c7c24fe893c21e8a4ef46eaf31badcab9f362841"),
            addr!("cd7492db29e2ab436e819b249452ee1bbdf52214"),
            addr!("cf0c122c6b73ff809c693db761e7baebe62b6a2e"),
            addr!("cf2f589bea4645c3ef47f1f33bebf100bee66e05"),
            addr!("cf8c23cf17bb5815d5705a15486fa83805415625"),
            addr!("d0834d08c83dbe216811aaea0eeffb2349e57634"),
            addr!("d0d3ebcad6a20ce69bc3bc0e1ec964075425e533"),
            addr!("d1afbccc9a2c2187ea544363b986ea0ab6ef08b5"),
            addr!("d375a513692336cf9eebce5e38869b447948016f"),
            addr!("d3f6571be1d91ac68b40daaa24075ca7e2f0f72e"),
            addr!("d50825f50384bc40d5a10118996ef503b3670afd"),
            addr!("d5281bb2d1ee94866b03a0fccdd4e900c8cb5091"),
            addr!("da1e53e088023fe4d1dc5a418581748f52cbd1b8"),
            addr!("dd339f370bbb18b8f389bd0443329d82ecf4b593"),
            addr!("decade1c6bf2cd9fb89afad73e4a519c867adcf5"), // Should be denied because can't approve more than balance
            addr!("dfdd3459d4f87234751696840092ee20c970fb07"),
            addr!("e0bdaafd0aab238c55d68ad54e616305d4a21772"),
            addr!("e2d66561b39eadbd488868af8493fb55d4b9d084"),
            addr!("e302bf71b1f6f3024e7642f9c824ac86b58436a0"),
            addr!("ea319e87cf06203dae107dd8e5672175e3ee976c"),
            addr!("ed5e5ab076ae60bdb9c49ac255553e65426a2167"),
            addr!("eeee2a622330e6d2036691e983dee87330588603"),
            addr!("ef5b32486ed432b804a51d129f4d2fbdf18057ec"),
            addr!("f1365ab39e192808b5301bcf6da973830e9e817f"),
            addr!("f198B4a2631B7D0B9FAc36f8B546Ed3DCe472A47"),
            addr!("fad45e47083e4607302aa43c65fb3106f1cd7607"),
            addr!("fcaa8eef70f373e00ac29208023d106c846259ee"),
            addr!("ff69e48af1174da7f15d0c771861c33d3f19ed8a"),
        ];

        // Of the deny listed tokens the following are detected as good:
        // - token 0xc12d1c73ee7dc3615ba4e37e4abfdbddfa38907e
        //   Has some kind of "freezing" mechanism where some balance is unusuable. We don't seem to
        //   trigger it.
        // - 0x910524678c0b1b23ffb9285a81f99c29c11cbaed
        //   Has some kind of time lock that we don't encounter.
        // - 0xed5e5ab076ae60bdb9c49ac255553e65426a2167
        //   Not sure why deny listed.
        // - 0x1337def18c680af1f9f45cbcab6309562975b1dd
        //   Not sure why deny listed, maybe the callback that I didn't follow in the SC code.
        // - 0x4f9254c83eb525f9fcf346490bbb3ed28a81c667
        //   Not sure why deny listed.

        let settlement = contracts::GPv2Settlement::deployed(&web3).await.unwrap();
        let finder = Arc::new(TokenOwnerFinder {
            web3: web3.clone(),
            proposers: vec![
                Arc::new(UniswapLikePairProviderFinder {
                    inner: uniswap_v2::get_liquidity_source(&web3).await.unwrap().0,
                    base_tokens: base_tokens.to_vec(),
                }),
                Arc::new(UniswapLikePairProviderFinder {
                    inner: sushiswap::get_liquidity_source(&web3).await.unwrap().0,
                    base_tokens: base_tokens.to_vec(),
                }),
                Arc::new(BalancerVaultFinder(
                    BalancerV2Vault::deployed(&web3).await.unwrap(),
                )),
                Arc::new(
                    UniswapV3Finder::new(
                        IUniswapV3Factory::deployed(&web3).await.unwrap(),
                        base_tokens.to_vec(),
                        FeeValues::Dynamic,
                    )
                    .await
                    .unwrap(),
                ),
                Arc::new(
                    BlockscoutTokenOwnerFinder::try_with_network(reqwest::Client::new(), 1)
                        .unwrap(),
                ),
            ],
        });
        let token_cache = TraceCallDetector {
            web3,
            finder,
            settlement_contract: settlement.address(),
        };

        println!("testing good tokens");
        for &token in base_tokens {
            let result = token_cache.detect(token).await;
            println!("token {:?} is {:?}", token, result);
        }

        println!("testing bad tokens");
        for &token in bad_tokens {
            let result = token_cache.detect(token).await;
            println!("token {:?} is {:?}", token, result);
        }
    }

    #[tokio::test]
    #[ignore]
    async fn mainnet_univ3() {
        //crate::tracing::initialize_for_tests("shared=debug");
        let http = create_env_test_transport();
        let web3 = Web3::new(http);
        let base_tokens = vec![testlib::tokens::WETH];
        let settlement = contracts::GPv2Settlement::deployed(&web3).await.unwrap();
        let factory = IUniswapV3Factory::deployed(&web3).await.unwrap();
        let univ3 = Arc::new(
            UniswapV3Finder::new(factory, base_tokens, FeeValues::Dynamic)
                .await
                .unwrap(),
        );
        let finder = Arc::new(TokenOwnerFinder {
            web3: web3.clone(),
            proposers: vec![univ3],
        });
        let token_cache = super::TraceCallDetector {
            web3,
            finder,
            settlement_contract: settlement.address(),
        };

        let result = token_cache.detect(testlib::tokens::USDC).await;
        dbg!(&result);
        assert!(result.unwrap().is_good());

        let only_v3_token = H160(hex!("f1b99e3e573a1a9c5e6b2ce818b617f0e664e86b"));
        let result = token_cache.detect(only_v3_token).await;
        dbg!(&result);
        assert!(result.unwrap().is_good());
    }
}
