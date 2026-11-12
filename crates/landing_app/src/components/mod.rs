//! The page's components.

mod agents;
mod chrome;
mod closing;
mod faq;
mod focus;
mod hero;
mod mark;
mod memory;
mod platforms;
mod relay;
mod section;
mod steps;

pub use agents::Agents;
pub use chrome::Footer;
pub use chrome::Nav;
pub use chrome::QUICK_START;
pub use chrome::REPOSITORY;
pub use closing::Closing;
pub use faq::Faq;
pub use focus::restore_focus;
pub use hero::Hero;
pub use mark::Lockup;
pub use mark::Mark;
pub use memory::Memory;
pub use platforms::Platforms;
pub use relay::Relay;
pub use section::Para;
pub use section::Section;
pub use steps::HowItWorks;
