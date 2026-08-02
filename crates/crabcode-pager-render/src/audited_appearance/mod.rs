//! Renderer-owned persisted preference value types.

mod config;
pub mod render_mermaid;
pub mod scroll_mode;
pub mod text_selection;

pub use config::{
    AnimationConfig, AppearanceConfig, BlockBackground, BlocksConfig, EditBlockConfig,
    ExecuteConfig, ExecuteHeaderStyle, FollowIndicator, LayoutConfig, ListDirConfig, PromptConfig,
    PromptViewConfig, RendererLanguage, ScrollConfig, ScrollbackConfig, ScrollbackDisplayConfig,
    ScrollbarConfig, ThinkingConfig, TodoBadgeFormat, TodoConfig, ToolBullet, ToolConfig,
    TurnStatusConfig,
};
