//! A demonstration of an offchain worker that sends onchain callbacks

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(test)]
mod tests;

use core::{convert::TryInto, fmt};
use frame_support::{
    debug, decl_error, decl_event, decl_module, decl_storage, dispatch::DispatchResult,
};
use parity_scale_codec::{Decode, Encode};

use frame_system::{
    self as system, ensure_none, ensure_signed,
    offchain::{
        AppCrypto, CreateSignedTransaction, SendSignedTransaction, SendUnsignedTransaction,
        SignedPayload, Signer, SigningTypes, SubmitTransaction,
    },
};
use sp_core::crypto::KeyTypeId;
use sp_runtime::{
    offchain as rt_offchain,
    offchain::{
        storage::StorageValueRef,
        storage_lock::{BlockAndTime, StorageLock},
    },
    transaction_validity::{
        InvalidTransaction, TransactionSource, TransactionValidity, ValidTransaction,
    },
    RuntimeDebug,
};
use sp_std::{cmp::min, collections::vec_deque::VecDeque, prelude::*, str};

use serde::{Deserialize, Deserializer};

/// Defines application identifier for crypto keys of this module.
///
/// Every module that deals with signatures needs to declare its unique identifier for
/// its crypto keys.
/// When an offchain worker is signing transactions it's going to request keys from type
/// `KeyTypeId` via the keystore to sign the transaction.
/// The keys can be inserted manually via RPC (see `author_insertKey`).
pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"demo");
pub const NUM_VEC_LEN: usize = 10;
/// The type to sign and send transactions.
pub const UNSIGNED_TXS_PRIORITY: u64 = 100;

// We are fetching information from the github public API about organization`substrate-developer-hub`.
pub const HTTP_REMOTE_REQUEST: &str = "https://api.github.com/orgs/substrate-developer-hub";
pub const HTTP_REMOTE_REQUEST_PRICE: &str = "https://api.coincap.io/v2/assets/polkadot";
pub const HTTP_HEADER_USER_AGENT: &str = "jimmychu0807";

pub const FETCH_TIMEOUT_PERIOD: u64 = 3000; // in milli-seconds
pub const LOCK_TIMEOUT_EXPIRATION: u64 = FETCH_TIMEOUT_PERIOD + 1000; // in milli-seconds
pub const LOCK_BLOCK_EXPIRATION: u32 = 3; // in block number

/// Based on the above `KeyTypeId` we need to generate a pallet-specific crypto type wrapper.
/// We can utilize the supported crypto kinds (`sr25519`, `ed25519` and `ecdsa`) and augment
/// them with the pallet-specific identifier.
pub mod crypto {
    use crate::KEY_TYPE;
    use sp_core::sr25519::Signature as Sr25519Signature;
    use sp_runtime::app_crypto::{app_crypto, sr25519};
    use sp_runtime::{traits::Verify, MultiSignature, MultiSigner};

    app_crypto!(sr25519, KEY_TYPE);

    pub struct TestAuthId;
    // implemented for ocw-runtime
    impl frame_system::offchain::AppCrypto<MultiSigner, MultiSignature> for TestAuthId {
        type RuntimeAppPublic = Public;
        type GenericSignature = sp_core::sr25519::Signature;
        type GenericPublic = sp_core::sr25519::Public;
    }

    // implemented for mock runtime in test
    impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature>
        for TestAuthId
    {
        type RuntimeAppPublic = Public;
        type GenericSignature = sp_core::sr25519::Signature;
        type GenericPublic = sp_core::sr25519::Public;
    }
}

#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug)]
pub struct Payload<Public> {
    number: u32,
    public: Public,
}

impl<T: SigningTypes> SignedPayload<T> for Payload<T::Public> {
    fn public(&self) -> T::Public {
        self.public.clone()
    }
}

// ref: https://serde.rs/container-attrs.html#crate
#[derive(Deserialize, Encode, Decode, Default)]
struct GithubInfo {
    // Specify our own deserializing function to convert JSON string to vector of bytes
    #[serde(deserialize_with = "de_string_to_bytes")]
    login: Vec<u8>,
    #[serde(deserialize_with = "de_string_to_bytes")]
    blog: Vec<u8>,
    public_repos: u32,
}

#[derive(Deserialize, Encode, Decode, Default)]
struct DotPrice {
    data: DotData,
    timestamp: u64,
}

#[derive(Deserialize, Encode, Decode, Default)]
struct DotData {
    #[serde(
        deserialize_with = "de_price_string_to_bytes",
        rename(deserialize = "priceUsd")
    )]
    price_usd: Vec<u8>,
}

pub fn de_price_string_to_bytes<'de, D>(de: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: &str = Deserialize::deserialize(de)?;
    if let Some(dot) = s.find('.') {
        let end = min(dot + 3, s.len());
        Ok(s[..end].as_bytes().to_vec())
    } else {
        Ok(s.as_bytes().to_vec())
    }
}

pub fn de_string_to_bytes<'de, D>(de: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: &str = Deserialize::deserialize(de)?;
    Ok(s.as_bytes().to_vec())
}

impl fmt::Debug for GithubInfo {
    // `fmt` converts the vector of bytes inside the struct back to string for
    //   more friendly display.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{{ login: {}, blog: {}, public_repos: {} }}",
            str::from_utf8(&self.login).map_err(|_| fmt::Error)?,
            str::from_utf8(&self.blog).map_err(|_| fmt::Error)?,
            &self.public_repos
        )
    }
}

impl fmt::Debug for DotPrice {
    // `fmt` converts the vector of bytes inside the struct back to string for
    //   more friendly display.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{{ USD Price: {} }}",
            str::from_utf8(&self.data.price_usd).map_err(|_| fmt::Error)?,
        )
    }
}

/// This is the pallet's configuration trait
pub trait Trait: system::Trait + CreateSignedTransaction<Call<Self>> {
    /// The identifier type for an offchain worker.
    type AuthorityId: AppCrypto<Self::Public, Self::Signature>;
    /// The overarching dispatch call type.
    type Call: From<Call<Self>>;
    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;
}

decl_storage! {
    trait Store for Module<T: Trait> as Example {
        /// A vector of recently submitted numbers. Bounded by NUM_VEC_LEN
        Numbers get(fn numbers): VecDeque<u32>;
        /// A vector of recently submitted USD Prices. Bounded by NUM_VEC_LEN
        UsdPrices get(fn usdprices): VecDeque<Vec<u8>>;
    }
}

decl_event!(
    /// Events generated by the module.
    pub enum Event<T>
    where
        AccountId = <T as system::Trait>::AccountId,
    {
        /// Event generated when a new number is accepted to contribute to the average.
        NewNumber(Option<AccountId>, u32),
        NewPrice(Option<AccountId>, Vec<u8>),
    }
);

decl_error! {
    pub enum Error for Module<T: Trait> {
        // Error returned when not sure which ocw function to executed
        UnknownOffchainMux,

        // Error returned when making signed transactions in off-chain worker
        NoLocalAcctForSigning,
        OffchainSignedTxError,

        // Error returned when making unsigned transactions in off-chain worker
        OffchainUnsignedTxError,

        // Error returned when making unsigned transactions with signed payloads in off-chain worker
        OffchainUnsignedTxSignedPayloadError,

        // Error returned when fetching github info
        HttpFetchingError,
    }
}

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        fn deposit_event() = default;

        // Use unsigned transaction, because I don't want to know who ( account Id )
        // sends this.
        #[weight = 10000]
        pub fn submit_price_unsigned(origin, price: Vec<u8>) -> DispatchResult {
            let _ = ensure_none(origin)?;
            // debug::info!("submit_number_unsigned: {}", number);
            Self::append_or_replace_price(price.clone());

            Self::deposit_event(RawEvent::NewPrice(None, price));
            Ok(())
        }

        fn offchain_worker(block_number: T::BlockNumber) {
            debug::info!("Entering off-chain worker");
            let result = Self::fetch_dot_usd_price();

            if let Err(e) = result {
                debug::error!("offchain_worker error: {:?}", e);
            }
        }
    }
}

impl<T: Trait> Module<T> {
    fn append_or_replace_price(price: Vec<u8>) {
        UsdPrices::mutate(|prices| {
            if prices.len() == NUM_VEC_LEN {
                let _ = prices.pop_front();
            }
            prices.push_back(price);
            debug::info!("Prices vector len: {}", prices.len());
        });
    }

    fn fetch_dot_usd_price() -> Result<(), Error<T>> {
        let s_info = StorageValueRef::persistent(b"offchain-demo::dot-price");
        // if let Some(Some(dot_price)) = s_info.get::<DotPrice>() {
        //     if dot_price.timestamp < {
        //         debug::info!("cached dot-price: {:?}", dot_price);
        //         return Ok(());
        //     }
        // }
        let mut lock = StorageLock::<BlockAndTime<Self>>::with_block_and_time_deadline(
            b"offchain-dot-price::lock",
            LOCK_BLOCK_EXPIRATION,
            rt_offchain::Duration::from_millis(LOCK_TIMEOUT_EXPIRATION),
        );

        if let Ok(_guard) = lock.try_lock() {
            match Self::fetch_n_parse_dot_usd_price() {
                Ok(dot_price) => {
                    s_info.set(&dot_price);
                    return Self::offchain_unsigned_tx_price(dot_price.data.price_usd);
                }
                Err(err) => return Err(err),
            }
        }

        Ok(())
    }

    /// Fetch from remote and deserialize the JSON to a struct
    fn fetch_n_parse_dot_usd_price() -> Result<DotPrice, Error<T>> {
        let resp_bytes = Self::fetch_from_remote_dot_usd_price().map_err(|e| {
            debug::error!("fetch_from_remote error: {:?}", e);
            <Error<T>>::HttpFetchingError
        })?;

        let resp_str = str::from_utf8(&resp_bytes).map_err(|_| <Error<T>>::HttpFetchingError)?;
        // Print out our fetched JSON string
        debug::info!("{}", resp_str);

        // Deserializing JSON to struct, thanks to `serde` and `serde_derive`
        let dot_price: DotPrice =
            serde_json::from_str(&resp_str).map_err(|_| <Error<T>>::HttpFetchingError)?;
        debug::info!("dot_price: {:?}", dot_price);
        Ok(dot_price)
    }

    /// This function uses the `offchain::http` API to query the remote github information,
    ///   and returns the JSON response as vector of bytes.
    fn fetch_from_remote_dot_usd_price() -> Result<Vec<u8>, Error<T>> {
        debug::info!("sending request to: {}", HTTP_REMOTE_REQUEST_PRICE);

        // Initiate an external HTTP GET request. This is using high-level wrappers from `sp_runtime`.
        let request = rt_offchain::http::Request::get(HTTP_REMOTE_REQUEST_PRICE);

        // Keeping the offchain worker execution time reasonable, so limiting the call to be within 3s.
        let timeout = sp_io::offchain::timestamp()
            .add(rt_offchain::Duration::from_millis(FETCH_TIMEOUT_PERIOD));

        // For github API request, we also need to specify `user-agent` in http request header.
        //   See: https://developer.github.com/v3/#user-agent-required
        let pending = request
            .add_header("User-Agent", HTTP_HEADER_USER_AGENT)
            .deadline(timeout) // Setting the timeout time
            .send() // Sending the request out by the host
            .map_err(|_| <Error<T>>::HttpFetchingError)?;

        // By default, the http request is async from the runtime perspective. So we are asking the
        //   runtime to wait here.
        // The returning value here is a `Result` of `Result`, so we are unwrapping it twice by two `?`
        //   ref: https://substrate.dev/rustdocs/v2.0.0/sp_runtime/offchain/http/struct.PendingRequest.html#method.try_wait
        let response = pending
            .try_wait(timeout)
            .map_err(|_| <Error<T>>::HttpFetchingError)?
            .map_err(|_| <Error<T>>::HttpFetchingError)?;

        if response.code != 200 {
            debug::error!("Unexpected http request status code: {}", response.code);
            return Err(<Error<T>>::HttpFetchingError);
        }

        // Next we fully read the response body and collect it to a vector of bytes.
        Ok(response.body().collect::<Vec<u8>>())
    }
    fn offchain_unsigned_tx_price(price: Vec<u8>) -> Result<(), Error<T>> {
        let call = Call::submit_price_unsigned(price);

        // `submit_unsigned_transaction` returns a type of `Result<(), ()>`
        //   ref: https://substrate.dev/rustdocs/v2.0.0/frame_system/offchain/struct.SubmitTransaction.html#method.submit_unsigned_transaction
        SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(call.into()).map_err(|_| {
            debug::error!("Failed in offchain_unsigned_tx");
            <Error<T>>::OffchainUnsignedTxError
        })
    }
}

impl<T: Trait> frame_support::unsigned::ValidateUnsigned for Module<T> {
    type Call = Call<T>;

    fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
        let valid_tx = |provide| {
            ValidTransaction::with_tag_prefix("ocw-demo")
                .priority(UNSIGNED_TXS_PRIORITY)
                .and_provides([&provide])
                .longevity(3)
                .propagate(true)
                .build()
        };

        match call {
            Call::submit_price_unsigned(_price) => valid_tx(b"submit_price_unsigned".to_vec()),
            _ => InvalidTransaction::Call.into(),
        }
    }
}

impl<T: Trait> rt_offchain::storage_lock::BlockNumberProvider for Module<T> {
    type BlockNumber = T::BlockNumber;
    fn current_block_number() -> Self::BlockNumber {
        <frame_system::Module<T>>::block_number()
    }
}
