#[cfg(feature = "analyze-protocol")]
pub(crate) use meaningful_data::*;

#[cfg(not(feature = "analyze-protocol"))]
pub(crate) use dummy_data::*;

#[cfg(feature = "analyze-protocol")]
mod meaningful_data {
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Eq, PartialEq, Serialize, Deserialize, Debug)]
    pub(crate) struct DebugRequest {
        exchange_id: u64,
    }

    impl DebugRequest {
        pub(crate) fn start() -> Self {
            let exchange_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            Self { exchange_id }
        }

        pub(crate) fn reply(&self) -> DebugResponse {
            DebugResponse {
                exchange_id: self.exchange_id,
            }
        }
    }

    #[derive(Eq, PartialEq, Serialize, Deserialize, Debug)]
    pub(crate) struct DebugResponse {
        exchange_id: u64,
    }

    impl DebugResponse {
        pub(crate) fn unsolicited() -> Self {
            let exchange_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            Self { exchange_id }
        }

        pub(crate) fn follow_up(&self) -> DebugRequest {
            DebugRequest {
                exchange_id: self.exchange_id,
            }
        }
    }
}

#[cfg(not(feature = "analyze-protocol"))]
mod dummy_data {
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Eq, PartialEq, Serialize, Deserialize, Debug)]
    pub(crate) struct DebugRequest {}

    impl DebugRequest {
        pub(crate) fn start() -> Self {
            Self {}
        }

        pub(crate) fn reply(&self) -> DebugResponse {
            DebugResponse {}
        }
    }

    #[derive(Eq, PartialEq, Serialize, Deserialize, Debug)]
    pub(crate) struct DebugResponse {}

    impl DebugResponse {
        pub(crate) fn unsolicited() -> Self {
            Self {}
        }

        pub(crate) fn follow_up(&self) -> DebugRequest {
            DebugRequest {}
        }
    }
}
