pub mod category;
pub mod video;

use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    video::configure(cfg);
    category::configure(cfg);
}
