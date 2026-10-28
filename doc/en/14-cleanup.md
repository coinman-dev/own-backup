# Disk cleanup (Windows)

The **Cleanup** section removes what programs and Windows leave behind:
temporary files, error reports, crash dumps, setup logs, caches, the
Recycle Bin and the traces of work — browser history, recent documents. The
Own Backup service does the work, so both the system's folders
and the users' are within its reach. There is no such section on Linux yet.

## How to use it

1. Tick the items. On the left are the groups and items, on the right the
   chosen item's description and the users whose folders are cleaned.
2. Press **Analyse**: every item shows its size, unticked ones too, and the
   bar at the bottom how much the ticked ones free. Nothing is removed. The
   **Files** button of an item opens the list of what will be removed: path,
   size, date, search, largest first or by path; for Windows traces, the
   registry entries too. Untick a file you need and it stays. The cleanup removes exactly
   that list for the ticked items: files that turned up after the analysis
   stay. The ticks may change after the analysis; changing the users or the
   days of history kept clears it — press **Analyse** again. For **Browser
   history** the description chooses how much history to keep: none, the last
   day, week or month.
3. Press **Clean up** and confirm with **Remove**. The items show the progress;
   the bar at the bottom shows the result — how much was freed and how many
   files stayed because programs hold them open. The result is recorded in
   **Activity**.

The chosen items and users are remembered.

## What is removed

| Item | Where | What stays |
|---|---|---|
| The user's temporary files | `AppData\Local\Temp` | files younger than a day |
| Windows temporary files | `Windows\Temp` | files younger than a day |
| Recycle Bin | the user's `$Recycle.Bin` on every drive | — |
| Windows error reports | `ProgramData\Microsoft\Windows\WER`, `AppData\Local\Microsoft\Windows\WER` | — |
| Crash dumps | `Windows\MEMORY.DMP`, `Windows\Minidump`, `AppData\Local\CrashDumps` | — |
| Windows setup logs | `Windows\Logs\CBS`, `DISM`, `WindowsUpdate` | logs younger than a week |
| WinINet cache | `AppData\Local\Microsoft\Windows\INetCache` | — |
| Shader cache | `D3DSCache`, the NVIDIA and AMD caches | — |
| Thumbnail cache (off by default) | `AppData\Local\Microsoft\Windows\Explorer` | files Explorer holds |
| Prefetch (off by default) | `Windows\Prefetch` | — |
| Windows Update cache | `Windows\SoftwareDistribution\Download` | files younger than a day; the update history `DataStore`; what is not installed yet Windows downloads again |
| Browser cache | the caches of Chrome, Edge, Vivaldi, Brave, Yandex, Opera, Firefox, Thunderbird in every profile | a browser that is open |
| Browser history (off by default) | `History` of Chromium browsers — whole, or what is older than the days kept (the database is edited on a copy and checked); `Visited Links`, `Top Sites`, `Shortcuts` | bookmarks, passwords; Firefox history (it shares a database with the bookmarks); a database left with a crash journal |
| Browser cookies (off by default) | `Cookies` of Chromium browsers, `cookies.sqlite` of Firefox and Thunderbird | a browser that is open |
| Browser sessions — open tabs (off by default) | `Sessions` of Chromium browsers, Firefox `sessionstore` | cookies (a separate item); a browser that is open. Firefox keeps its until-closed cookies in the session — sites that keep you signed in only with those ask you to sign in again |
| Browser crash reports | `Crashpad\reports` of Chromium browsers; `Crash Reports\pending`, `submitted` and `minidumps` of Firefox and Thunderbird | a browser that is open |
| Recent documents (off by default) | the shortcuts `AppData\Roaming\Microsoft\Windows\Recent\*.lnk`; in the registry `Explorer\RecentDocs`, `ComDlg32\OpenSavePidlMRU`, `OpenSaveMRU`, `LastVisitedPidlMRU` | the documents themselves |
| Jump lists (off by default) | `Recent\AutomaticDestinations`, `Recent\CustomDestinations` | the folders pinned to Explorer's Quick access |
| Run and Explorer history (off by default) | in the registry `Explorer\RunMRU`, `TypedPaths`, `WordWheelQuery` | — |
| Remote Desktop connections (off by default) | in the registry `Terminal Server Client\Default` (`MRU*`), `Servers`; `Documents\Default.rdp` | saved passwords (Credential Manager), other `.rdp` files |
| Microsoft Office recent documents (off by default) | in the registry `Office\<version>\<program>\File MRU`, `Place MRU`, `User MRU\<account>` too — `Item N` only; the shortcuts in `AppData\Roaming\Microsoft\Office\Recent` | the places pinned to the lists; an Office program that is open |
| Programs' recent files (off by default) | in the registry: Paint, WordPad, Windows Media Player, MPC-HC (`Recent File List`, `MediaHistory`, `PlaylistHistory`), 7-Zip (`FolderHistory`, `CopyHistory`, `PanelPath*`, `ArcHistory`, `PathHistory`), WinRAR, Adobe Acrobat Reader, Registry Editor's `LastKey` | the programs' settings (7-Zip's folder shortcuts, MPC-HC's settings); a program that is open |
| Programs' caches | XnView MP's thumbnails (`Thumb.db`), the caches of Discord, Visual Studio Code, `Steam\htmlcache` | a program that is open |
| Telegram Desktop cache (off by default) | `tdata\user_data*\cache`, `media_cache` of every account | the sign-in and the chats; Telegram open |
| npm, pip, Yarn and NuGet caches (off by default) | `npm-cache\_cacache`, `pip\cache`, `Yarn\Cache`, `NuGet\v3-cache` | — |
| Programs' logs | the log folders of qBittorrent, Zed, DaVinci Resolve, OBS Studio, npm | logs younger than a week; DaVinci Resolve's projects and project library; a program that is open |
| NVIDIA driver installers (off by default) | `C:\NVIDIA\DisplayDriver`, `ProgramData\NVIDIA Corporation\Downloader\*` | younger than a day; the installed driver |

### Files by masks

The **Files by masks** item (off by default) looks for abandoned files by
name masks — like Ace Utilities' junk cleaning, but more carefully:

- **Where.** In the ticked users' profiles; a window run as an
  administrator may add other folders and drives ("Where else to search").
- **Never entered:** `Windows`, `Program Files`, `ProgramData`, `AppData`
  (programs' junk is the other items' business), `.git`, `node_modules`,
  cloud folders (OneDrive, Dropbox, Google Drive, Yandex Disk — a removal
  there would remove the file in the cloud too), links and junctions. This
  program's files (`.own-backup-*`) and backup images (`.obk`, `.vhdx`,
  `.qcow2`) are never touched, whatever the masks.
- **Masks** — a list as in Ace Utilities (the "Masks…" button in the
  item's description): the mask, its kind and what it finds. Temporary
  files and junk are on (`*.tmp`, `*.temp`, `~$*`, `*.~*`, `*.$$$`,
  `Thumbs.db`, `*.dmp`, `*.gid` …); off are backups (`*.bak`, `*.old`,
  `*.prv`, `*.wbk` — your documents and programs' keys turn up there), logs
  (`*.log`, `*log.txt`), pieces of files after a disk check (`*.chk`), a
  build's intermediate files and `*.tmp*` (it takes `.tmpl` templates).
  Own masks can be added. A mask is a file name only, and one that does not
  take ordinary files: `*.*`, `*.d*` (it would catch `.docx`), `K*` are
  refused.
- **Age.** Files older than a day, a week (the default), a month or three
  months go.
- **Only after a look.** The item is cleaned only once its **Files** list
  was opened after the analysis; the service does not remove it without
  an analysis. Each file of the list has a tick — untick it and the file
  stays. Folders are not removed.

## What is guaranteed

- **The removal is plain and fast.** Files do not go to the Recycle Bin and
  are not overwritten; they cannot be brought back.
- **Only the folders listed.** The cleanup does not search the whole disk by
  masks: a mask such as `*.prv` or `*.bak` catches programs' keys and backup
  copies of documents.
- **Recent files are left.** In the temporary folders only files unchanged for
  more than a day are removed (the later of the write and creation times
  counts), so what a program or a setup uses now stays.
- **Links and junctions are not followed.** What they point to stays intact;
  cloud placeholders (OneDrive) are not touched either.
- **Files in use are skipped** and counted apart.
- **Other users' folders — only from an administrator's window.** An ordinary
  user cleans only their own folders; a window run as an administrator may
  tick any user of the computer.
- Restore points and the Windows event logs are not removed.
- **An open browser is left.** Its folders are skipped, and the item says
  "close: …". Edge may keep running in the background after its window is
  closed — then turn off "Continue running background extensions and apps"
  in its settings.
- Browsers' passwords, bookmarks and autofill are never removed.
- **Of programs, only their caches, logs and recent lists.** Their settings
  and data — DaVinci Resolve's projects and project library, Notepad++'s
  unsaved documents, qBittorrent's torrents, the Telegram sign-in — are not
  touched. A program that is open is left, and the item says "close: …".
- **In the registry, only the lists named.** The values and subkeys of those
  keys go; the keys themselves and their default values stay. The service
  opens a signed-in user's registry in `HKEY_USERS`; for a user who is not
  signed in it loads their `NTUSER.DAT` for itself alone and only for the
  time the reading or removal takes. If the file is in use (the user is
  signing in just then), the item is left and counted apart. In the **Files**
  list an entry reads `HKEY_USERS\<SID>\…\RunMRU\a = cmd\1` — after `=` is
  what it holds.
