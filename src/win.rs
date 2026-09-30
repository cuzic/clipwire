use super::*;

// ── Windows clipboard implementation ──────────────────────────────────────────

#[cfg(windows)]
pub(crate) mod win_clip {
    use super::ClipKind;
    use anyhow::{bail, Context, Result};
    use windows::{
        core::w,
        Win32::{
            Foundation::{GetLastError, HANDLE, HGLOBAL, WIN32_ERROR},
            System::{
                Com::{
                    CoInitializeEx, CoUninitialize, IStream, COINIT_APARTMENTTHREADED,
                    COINIT_MULTITHREADED, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL,
                    TYMED_ISTREAM,
                },
                DataExchange::{
                    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
                    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
                },
                Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
                Ole::{OleGetClipboard, OleInitialize, ReleaseStgMedium},
                Threading::CreateMutexW,
            },
            UI::Shell::{DragQueryFileW, HDROP},
        },
    };

    const CF_DIB: u32 = 8;
    const CF_WAVE: u32 = 12;
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP: u32 = 15;

    struct ClipGuard;
    impl ClipGuard {
        fn open() -> Result<Self> {
            unsafe { OpenClipboard(None)? };
            Ok(ClipGuard)
        }
    }
    impl Drop for ClipGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    unsafe fn fmt_avail(fmt: u32) -> bool {
        IsClipboardFormatAvailable(fmt).is_ok()
    }

    unsafe fn reg_fmt(name: windows::core::PCWSTR) -> u32 {
        RegisterClipboardFormatW(name)
    }

    /// `GlobalLock` が失敗（null 返却）した場合、null ポインタから
    /// `from_raw_parts` するのは未定義動作（Rust の panic にはならず、
    /// catch_unwind でも panic hook でも捕まえられない segfault 相当の
    /// クラッシュになりうる）。ここで明示的に弾く。
    unsafe fn hglobal_bytes(h: HANDLE) -> Vec<u8> {
        let hg = HGLOBAL(h.0);
        let size = GlobalSize(hg);
        let ptr = GlobalLock(hg);
        if ptr.is_null() {
            tracing::warn!("hglobal_bytes: GlobalLock が null を返しました (size={size})");
            return Vec::new();
        }
        let data = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _ = GlobalUnlock(hg);
        data
    }

    pub unsafe fn read_clipboard() -> ClipKind {
        let fmt_fgd = reg_fmt(w!("FileGroupDescriptorW"));
        let fmt_fc = reg_fmt(w!("FileContents"));
        let fmt_html = reg_fmt(w!("HTML Format"));
        let fmt_url = reg_fmt(w!("UniformResourceLocatorW"));
        let fmt_rtf = reg_fmt(w!("Rich Text Format"));

        if fmt_avail(CF_DIB) {
            if let Ok(d) = read_image() {
                return ClipKind::Image(d);
            }
        }
        if fmt_avail(CF_HDROP) {
            if let Ok(v) = read_files() {
                if !v.is_empty() {
                    return ClipKind::Files(v);
                }
            }
        }
        if fmt_avail(fmt_fgd) && fmt_avail(fmt_fc) {
            if let Ok(v) = read_vfile_names() {
                if !v.is_empty() {
                    return ClipKind::VFiles(v);
                }
            }
        }
        if fmt_avail(CF_WAVE) {
            if let Ok(d) = read_wave() {
                return ClipKind::Audio(d);
            }
        }
        if fmt_avail(fmt_url) {
            if let Ok(u) = read_url(fmt_url) {
                return ClipKind::Url(u);
            }
        }
        if fmt_avail(fmt_html) {
            if let Ok(h) = read_html(fmt_html) {
                return ClipKind::Html(h);
            }
        }
        if fmt_avail(fmt_rtf) {
            if let Ok(r) = read_rtf(fmt_rtf) {
                return ClipKind::Rtf(r);
            }
        }
        if fmt_avail(CF_UNICODETEXT) {
            if let Ok(t) = read_text() {
                if !t.is_empty() {
                    return ClipKind::Text(t);
                }
            }
        }
        ClipKind::Empty
    }

    unsafe fn read_image() -> Result<Vec<u8>> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(CF_DIB).context("CF_DIB")?;
        dib_to_png(&hglobal_bytes(h))
    }

    fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
        if dib.len() < 40 {
            bail!("DIB too short");
        }
        let info_size = u32::from_le_bytes(dib[0..4].try_into()?) as usize;
        let bpp = u16::from_le_bytes(dib[14..16].try_into()?) as usize;
        let clr_count = if bpp <= 8 { 1usize << bpp } else { 0 };
        let pix_offset = 14 + info_size + clr_count * 4;
        let file_size = 14 + dib.len();

        let mut bmp = Vec::with_capacity(file_size);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&(pix_offset as u32).to_le_bytes());
        bmp.extend_from_slice(dib);

        let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp)?;
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        Ok(png)
    }

    unsafe fn read_files() -> Result<Vec<String>> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(CF_HDROP).context("CF_HDROP")?;
        let hdrop = HDROP(h.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            paths.push(String::from_utf16_lossy(&buf[..len]));
        }
        Ok(paths)
    }

    unsafe fn read_vfile_names() -> Result<Vec<String>> {
        let fmt = reg_fmt(w!("FileGroupDescriptorW"));
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(fmt).context("FileGroupDescriptorW")?;
        parse_fgd(&hglobal_bytes(h))
    }

    fn parse_fgd(data: &[u8]) -> Result<Vec<String>> {
        if data.len() < 4 {
            bail!("FGD too short");
        }
        let count = u32::from_le_bytes(data[0..4].try_into()?) as usize;
        const ENTRY: usize = 592;
        const NAME: usize = 72;
        let mut names = Vec::with_capacity(count);
        for i in 0..count {
            let base = 4 + i * ENTRY;
            if base + ENTRY > data.len() {
                break;
            }
            let wdata: &[u8] = &data[base + NAME..base + NAME + 520];
            let words: Vec<u16> = wdata
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
            names.push(String::from_utf16_lossy(&words[..end]));
        }
        Ok(names)
    }

    pub unsafe fn get_vfile_contents(index: usize) -> Result<Vec<u8>> {
        let fmt_fc = reg_fmt(w!("FileContents"));
        let _ = OleInitialize(None);

        let data_obj = OleGetClipboard()?;

        let mut fetc = FORMATETC {
            cfFormat: fmt_fc as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: index as i32,
            tymed: TYMED_ISTREAM.0 as u32,
        };

        if let Ok(mut stgm) = data_obj.GetData(&fetc) {
            if stgm.tymed == TYMED_ISTREAM.0 as u32 {
                let data = {
                    let stream: &Option<IStream> = &stgm.u.pstm;
                    if let Some(s) = stream {
                        drain_istream(s)?
                    } else {
                        bail!("null IStream")
                    }
                };
                ReleaseStgMedium(&mut stgm);
                return Ok(data);
            }
            ReleaseStgMedium(&mut stgm);
        }

        fetc.tymed = TYMED_HGLOBAL.0 as u32;
        let mut stgm = data_obj.GetData(&fetc)?;
        let hg = stgm.u.hGlobal;
        let size = GlobalSize(hg);
        let ptr = GlobalLock(hg);
        if ptr.is_null() {
            ReleaseStgMedium(&mut stgm);
            bail!("get_vfile_contents: GlobalLock が null を返しました (size={size})");
        }
        let data = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _ = GlobalUnlock(hg);
        ReleaseStgMedium(&mut stgm);
        Ok(data)
    }

    unsafe fn drain_istream(stream: &IStream) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            let mut read = 0u32;
            let _ = stream.Read(
                chunk.as_mut_ptr() as *mut _,
                chunk.len() as u32,
                Some(&mut read),
            );
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read as usize]);
        }
        Ok(buf)
    }

    unsafe fn read_wave() -> Result<Vec<u8>> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(CF_WAVE).context("CF_WAVE")?;
        Ok(hglobal_bytes(h))
    }

    unsafe fn read_html(fmt: u32) -> Result<String> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(fmt).context("HTML Format")?;
        let data = hglobal_bytes(h);
        let hdr = String::from_utf8_lossy(&data[..data.len().min(512)]);
        let start = hdr
            .lines()
            .find(|l| l.starts_with("StartHTML:"))
            .and_then(|l| l[10..].trim().parse::<usize>().ok())
            .unwrap_or(0);
        Ok(String::from_utf8_lossy(if start < data.len() {
            &data[start..]
        } else {
            &data
        })
        .into_owned())
    }

    unsafe fn read_url(fmt: u32) -> Result<String> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(fmt).context("URL format")?;
        let data = hglobal_bytes(h);
        let words: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
        Ok(String::from_utf16_lossy(&words[..end]).trim().to_string())
    }

    unsafe fn read_rtf(fmt: u32) -> Result<String> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(fmt).context("RTF")?;
        Ok(String::from_utf8_lossy(&hglobal_bytes(h)).into_owned())
    }

    unsafe fn read_text() -> Result<String> {
        let _g = ClipGuard::open()?;
        let h = GetClipboardData(CF_UNICODETEXT).context("CF_UNICODETEXT")?;
        let data = hglobal_bytes(h);
        let words: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
        Ok(String::from_utf16_lossy(&words[..end]))
    }

    pub unsafe fn write_clipboard_text(text: &str) -> Result<()> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_len = wide.len() * 2;

        let hg = GlobalAlloc(GMEM_MOVEABLE, byte_len)?;
        let ptr = GlobalLock(hg) as *mut u16;
        if ptr.is_null() {
            bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(hg);

        OpenClipboard(None)?;
        EmptyClipboard()?;
        if let Err(e) = SetClipboardData(CF_UNICODETEXT, HANDLE(hg.0)) {
            let _ = CloseClipboard();
            bail!("SetClipboardData: {e}");
        }
        CloseClipboard()?;
        Ok(())
    }

    pub fn show_balloon(msg: &str) {
        let msg = msg.to_string();
        std::thread::spawn(move || {
            if let Err(e) = show_toast(&msg) {
                let error = format!("show_balloon: toast の表示に失敗しました: {e}\n");
                tracing::error!("{}", error.trim());
                let _ = append_toast_log(&error);
            }
        });
    }

    fn append_toast_log(msg: &str) -> Result<()> {
        let path = super::clipwire_config_dir().join("toast.log");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(msg.as_bytes())?;
        Ok(())
    }

    fn show_toast(msg: &str) -> Result<()> {
        use windows::{
            core::HSTRING,
            Data::Xml::Dom::XmlDocument,
            UI::Notifications::{ToastNotification, ToastNotificationManager},
        };

        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        let xml_str = format!(
            r#"<toast><visual><binding template="ToastGeneric"><text>clipwire</text><text>{}</text></binding></visual></toast>"#,
            super::xml_escape(msg),
        );
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(xml_str))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(CLIPWIRE_AUMID))?
            .Show(&toast)?;
        unsafe { CoUninitialize() };
        Ok(())
    }

    /// `register` 要求到着時に WinRT toast（承認/拒否ボタン付き）を表示する。
    /// 「承認」クリック → 同プロセス内の Activated ハンドラが registered.toml に直接書き込む。
    pub fn show_register_toast(
        name: String,
        entry: super::StoredTarget,
        config_dir: std::path::PathBuf,
        reapproval: bool,
    ) {
        let log_path = config_dir.join("toast.log");
        std::thread::spawn(move || {
            if let Err(e) =
                show_register_toast_impl(name.clone(), entry, config_dir.clone(), reapproval)
            {
                let msg = format!(
                    "[clipwire] register toast error for '{}': {e}; run `clipwire approve {}` to approve manually\n",
                    name, name
                );
                eprintln!("{}", msg.trim());
                let _ = std::fs::write(&log_path, &msg);
                tracing::error!("{}", msg.trim());
            }
        });
    }

    fn show_register_toast_impl(
        name: String,
        entry: super::StoredTarget,
        config_dir: std::path::PathBuf,
        reapproval: bool,
    ) -> Result<()> {
        // 実行確認用: 関数が呼ばれたら必ずログに記録する
        let log_path = config_dir.join("toast.log");
        let _ = std::fs::write(
            &log_path,
            format!(
                "[clipwire] show_register_toast_impl called for '{}'\n",
                name
            ),
        );

        use windows::{
            core::{Interface, HSTRING},
            Data::Xml::Dom::XmlDocument,
            Foundation::TypedEventHandler,
            UI::Notifications::{
                ToastActivatedEventArgs, ToastDismissedEventArgs, ToastNotification,
                ToastNotificationManager,
            },
        };

        // MTA で WinRT を初期化
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }

        let body = if reapproval {
            format!("'{}' の設定が変更されました。再承認しますか？", name)
        } else {
            format!("'{}' を承認しますか？", name)
        };
        let xml_str = format!(
            r#"<toast><visual><binding template="ToastGeneric"><text>clipwire: 登録要求</text><text>{}</text></binding></visual><actions><action content="承認" arguments="approve"/><action content="拒否" arguments="deny"/></actions></toast>"#,
            super::xml_escape(&body),
        );

        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(xml_str.as_str()))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;

        let (tx, rx) = std::sync::mpsc::channel::<bool>();

        {
            let tx = tx.clone();
            toast.Activated(&TypedEventHandler::<
                ToastNotification,
                windows::core::IInspectable,
            >::new(move |_, args| {
                let approved = args
                    .as_ref()
                    .and_then(|a| a.cast::<ToastActivatedEventArgs>().ok())
                    .and_then(|a| a.Arguments().ok())
                    .map(|s| s == "approve")
                    .unwrap_or(false);
                let _ = tx.send(approved);
                Ok(())
            }))?;
        }
        {
            let tx = tx.clone();
            toast.Dismissed(&TypedEventHandler::<
                ToastNotification,
                ToastDismissedEventArgs,
            >::new(move |_, _| {
                let _ = tx.send(false);
                Ok(())
            }))?;
        }

        // スタートメニューのショートカットで登録済みの AUMID を使う
        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(CLIPWIRE_AUMID))?;
        let _ = std::fs::write(
            &log_path,
            format!("[clipwire] calling notifier.Show() for '{}'\n", name),
        );
        notifier.Show(&toast)?;
        let _ = std::fs::write(
            &log_path,
            format!("[clipwire] notifier.Show() succeeded for '{}'\n", name),
        );

        // ユーザー操作を最大 10 分待機
        if rx
            .recv_timeout(std::time::Duration::from_secs(600))
            .unwrap_or(false)
        {
            let registered_path = config_dir.join("registered.toml");
            let pending_path = config_dir.join("pending.toml");
            let mut registered = super::load_target_map_or_warn(&registered_path);
            let mut pending = super::load_target_map_or_warn(&pending_path);
            registered.insert(name.clone(), entry);
            pending.remove(&name);
            super::save_target_map(&registered_path, &registered)?;
            super::save_target_map(&pending_path, &pending)?;
            eprintln!("[clipwire] '{}' 承認 → registered.toml", name);
            show_balloon(&format!("'{}' を承認しました", name));
        }

        unsafe {
            CoUninitialize();
        }
        Ok(())
    }

    const CLIPWIRE_AUMID: &str = "cuzic.clipwire";

    /// サーバー起動時に一度だけ呼ぶ。
    /// プロセスに AUMID を設定することで CreateToastNotifierWithId が使えるようになる。
    pub fn ensure_aumid_registered() {
        unsafe {
            if let Err(e) = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
                windows::core::w!("cuzic.clipwire"),
            ) {
                eprintln!("[clipwire] SetCurrentProcessExplicitAppUserModelID failed: {e}");
            }
        }
    }

    /// `-WindowStyle Hidden` では eprintln! はどこにも表示されないため、直接
    /// ログファイルへ書く。`tracing::error!` は非同期（non-blocking writer）
    /// で実際の書き込みはバックグラウンドスレッドが行うため、直後の
    /// `std::process::exit`（デストラクタを一切走らせない）と組み合わせると
    /// メッセージが失われうる——ここでは同期的にファイルへ書いてから終了する。
    fn log_and_exit(msg: &str) -> ! {
        eprintln!("{msg}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(super::log_file_path())
        {
            use std::io::Write as _;
            let _ = writeln!(f, "{msg}");
        }
        std::process::exit(1);
    }

    pub unsafe fn acquire_mutex() -> windows::Win32::Foundation::HANDLE {
        match CreateMutexW(None, true, w!("Global\\clipwire_singleton")) {
            Ok(h) => {
                if GetLastError() == WIN32_ERROR(183) {
                    log_and_exit("clipwire serve は既に起動中です。");
                }
                h
            }
            Err(e) => log_and_exit(&format!("CreateMutexW failed: {e}")),
        }
    }

    pub fn sta_loop(rx: std::sync::mpsc::Receiver<super::ClipRequest>) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            use super::ClipRequest;
            for req in rx {
                match req {
                    ClipRequest::GetClip { reply } => {
                        let _ = reply.send(read_clipboard());
                    }
                    ClipRequest::SetClip { text, reply } => {
                        let _ = reply.send(write_clipboard_text(&text));
                    }
                    ClipRequest::GetFile { path, reply } => {
                        let _ = reply.send(std::fs::read(&path).ok());
                    }
                    ClipRequest::GetVFile { index, reply } => {
                        let _ = reply.send(get_vfile_contents(index).ok());
                    }
                }
            }
            CoUninitialize();
        }
    }
}
