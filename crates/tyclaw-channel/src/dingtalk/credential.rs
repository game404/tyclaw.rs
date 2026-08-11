//! 兼容层：凭证与 Token 实现位于 `tyclaw-tools`，供渠道与主动发送工具共享缓存。

pub use tyclaw_tools::{
    DingTalkCredential as Credential, DingTalkTokenManager as TokenManager,
};
