# Disk cleanup (Windows)

The **Cleanup** section removes what programs and Windows leave behind:
temporary files, error reports, crash dumps, setup logs, caches and the
Recycle Bin. The Own Backup service does the work, so both the system's folders
and the users' are within its reach. There is no such section on Linux yet.

## How to use it

1. Tick the items. On the left are the groups and items, on the right the
   chosen item's description and the users whose folders are cleaned.
2. Press **Analyse**: every item shows its size, unticked ones too, and the
   bar at the bottom how much the ticked ones free. Nothing is removed. The
   **Files** button of an item opens the list of what will be removed: path,
   size, date, search, largest first or by path. The cleanup removes exactly
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
| Browser cache | the caches of Chrome, Edge, Vivaldi, Brave, Yandex, Opera, Firefox, Thunderbird in every profile | a browser that is open |
| Browser history (off by default) | `History` of Chromium browsers — whole, or what is older than the days kept (the database is edited on a copy and checked); `Visited Links`, `Top Sites`, `Shortcuts` | bookmarks, passwords; Firefox history (it shares a database with the bookmarks); a database left with a crash journal |
| Browser cookies (off by default) | `Cookies` of Chromium browsers, `cookies.sqlite` of Firefox and Thunderbird | a browser that is open |
| Saved tabs (off by default) | `Sessions` of Chromium browsers, Firefox `sessionstore` | a browser that is open |

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
