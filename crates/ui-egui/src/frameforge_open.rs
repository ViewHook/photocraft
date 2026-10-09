//! FrameForge open seam. Server-provided fonts can later supply ImportOptions' resolver.
use crate::PhotocraftApp;
pub(crate) fn is_frameforge(name: &str) -> bool {
    std::path::Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case("frameforge"))
}
pub(crate) fn open(app: &mut PhotocraftApp, name: &str, bytes: &[u8]) -> Result<Vec<String>, String> {
    let archive = photocraft_frameforge::read_archive(bytes)?;
    #[cfg(not(target_arch = "wasm32"))]
    photocraft_text::shared().lock().unwrap_or_else(|e| e.into_inner()).fonts.load_system_fonts();
    let report = photocraft_frameforge::import_into(&mut app.session, &archive, &photocraft_frameforge::ImportOptions::default())?;
    app.sync_views();
    app.ui.status = format!("Opened {name}");
    app.ui.status_error = false;
    crate::notices::io_warnings(app, &format!("Opened {name}"), &report.warnings);
    photocraft_engine::automate_cmds::document_opened(&mut app.session);
    Ok(report.warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extension_matching() {
        assert!(is_frameforge("DESIGN.FRAMEFORGE"));
        assert!(!is_frameforge("photo.png"));
    }
    #[test]
    fn invalid_archive_preserves_document() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.session.execute("file.new", serde_json::json!({"width":100,"height":100})).unwrap();
        app.background_jobs = true;
        assert!(app.open_bytes("bad.frameforge", b"invalid ZIP").is_err());
        assert_eq!(app.session.documents().len(), 1);
        assert!(app.jobs.opens.is_empty());
    }
}
