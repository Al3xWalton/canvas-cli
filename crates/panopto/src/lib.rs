//! Panopto ReCap support: find recordings, resolve streams, download them.
//!
//! A recording is identified by its delivery ID. With the session's
//! cookies, `POST /Panopto/Pages/Viewer/DeliveryInfo.aspx` (sending the
//! `csrfToken` cookie as `X-Csrf-Token`) returns the HLS stream URL,
//! which `ffmpeg -c copy` remuxes to MP4 without re-encoding.
//!
//! Status: scaffold. Stream lookup lands in issue #5.
