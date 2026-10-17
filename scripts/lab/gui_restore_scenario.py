"""The restore screen of the boot medium, driven the way a person drives it
(CHECKS 3): RESTORE -> Add backup -> File -> the in-app picker -> the copy ->
password -> target disk -> bootable -> place on the free space -> Restore ->
plan check -> boot entries -> typed YES -> progress to the end.

Called by run-system-obk.py `restore-gui` with the host address of the
medium's DevTools relay. Records what the screen said at every step, with
screenshots, and judges the criteria of CHECKS 3:

* before the password: never the false "disk ... is not connected";
* while the copy is read: a loading state is shown;
* the plan check passes and the boot entries are offered;
* the restore completes with every operation done.
"""
import json
import time
import urllib.error

import cdp

HELPERS = r"""
window.__ob = {
  all(sel) { return Array.from(document.querySelectorAll(sel)); },
  txt(el) { return (el && (el.innerText || el.textContent) || '').trim(); },
  byText(sel, text) {
    const t = text.toLowerCase();
    return this.all(sel).find(e => this.txt(e).toLowerCase().includes(t)) || null;
  },
  click(el) { if (!el) return false; el.scrollIntoView({block: 'center'}); el.click(); return true; },
  setInput(el, value) {
    const proto = Object.getPrototypeOf(el);
    const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
    setter.call(el, value);
    el.dispatchEvent(new Event('input', {bubbles: true}));
    el.dispatchEvent(new Event('change', {bubbles: true}));
  },
  lastDialog(title) {
    const d = this.all('.overlay .dialog, .modal-backdrop').filter(x => !title || this.txt(x).includes(title));
    return d[d.length - 1] || null;
  },
  state() {
    const q = s => this.txt(document.querySelector(s));
    return {
      loading: q('.loading-note'), key: q('.key-note'), warn: q('.warn-note'),
      modal: !!document.querySelector('.modal-backdrop'), busy: q('.modal-backdrop p.note.busy'),
      opening: q('.modal-backdrop .modal-footer .btn-primary'), err: q('.modal-backdrop p.err'),
    };
  },
};
true
"""

NOT_CONNECTED = 'is not connected'


def connect(addr: str, deadline_s: float = 900):
    host, port = addr.split(':')
    deadline = time.monotonic() + deadline_s
    while time.monotonic() < deadline:
        try:
            pages = [t for t in cdp.targets(host, int(port)) if t.get('type') == 'page']
            if pages:
                ws = pages[0]['webSocketDebuggerUrl'].replace('127.0.0.1:9222', addr).replace('localhost:9222', addr)
                page = cdp.Cdp(ws)
                if page.eval("!!document.querySelector('.nav-item')"):
                    return page, pages[0]
                page.sock.close()
        except (OSError, urllib.error.URLError, ConnectionError, RuntimeError, ValueError):
            pass
        time.sleep(3)
    raise TimeoutError('the interface did not come up with DevTools within 15 minutes')


def run(addr, shots, budget, screendump) -> str:
    log = []
    report = {'steps': log}

    def note(step, **data):
        row = {'t': round(time.time(), 1), 'step': step, **data}
        log.append(row)
        print('GUI', json.dumps(row, ensure_ascii=False)[:400], flush=True)
        (shots / 'scenario.json').write_text(json.dumps(report, ensure_ascii=False, indent=1) + '\n')

    def shot(name):
        try:
            page.screenshot(shots / f'{len(log):02d}-{name}.png')
        except Exception as error:
            note('screenshot-failed', name=name, error=str(error))
        budget()

    page, target = connect(addr, float(__import__('os').environ.get('OBK_CDP_WAIT', '900')))
    page.eval(HELPERS)
    note('connected', url=target.get('url'), title=target.get('title'))
    shot('start')

    # 1. The restore screen.
    assert page.eval("__ob.click(__ob.byText('.nav-item', 'restore'))"), 'no RESTORE in the sidebar'
    page.wait("!!document.querySelector('button.add-btn')", 60)
    shot('restore-screen')

    # 2. Add backup -> File -> the in-app picker.
    page.eval("__ob.click(document.querySelector('button.add-btn'))")
    page.wait("!!__ob.byText('.dialog button.choice', 'file')", 30)
    page.eval("(() => { const r = document.querySelector('.dialog label.remember input'); if (r && r.checked) r.click(); })()")
    page.eval("__ob.click(__ob.byText('.dialog button.choice', 'file'))")
    page.wait("!!document.querySelector('input.path')", 30)
    shot('picker')
    found = None
    for letter in 'CDEFGHIJ':
        page.eval(f"""(() => {{ const i = document.querySelector('input.path');
            __ob.setInput(i, '{letter}:\\\\obk');
            i.dispatchEvent(new KeyboardEvent('keydown', {{key: 'Enter', bubbles: true}})); }})()""")
        time.sleep(2)
        page.wait("!__ob.byText('.muted', 'reading')", 60)
        found = page.eval("(() => { const r = __ob.all('div.row').find(x => __ob.txt(x.querySelector('.nm')).toLowerCase().endsWith('.obk')); return r ? __ob.txt(r.querySelector('.nm')) : null; })()")
        if found:
            note('picker-found', folder=f'{letter}:\\obk', file=found)
            break
    assert found, 'no .obk found by the picker on C:..J:'
    page.eval("__ob.click(__ob.all('div.row').find(x => __ob.txt(x.querySelector('.nm')).toLowerCase().endsWith('.obk')))")
    page.wait("!document.querySelector('.foot button.btn.primary').disabled", 15)
    page.eval("__ob.click(document.querySelector('.foot button.btn.primary'))")

    # 3. Before the password: sample what the dashboard says.
    seen = []
    t0 = time.monotonic()
    while time.monotonic() - t0 < 20:
        st = page.eval('__ob.state()')
        if not seen or st != seen[-1]:
            seen.append(st)
        if st.get('key'):
            break
        time.sleep(0.3)
    false_alarm = any(NOT_CONNECTED in (s.get('warn') or '') for s in seen)
    note('before-password', states=seen, false_not_connected=false_alarm)
    shot('before-password')
    assert not false_alarm, 'the false "disk is not connected" appeared before the password'
    page.wait("!!document.querySelector('.key-note')", 60)

    # 4. Password.
    page.eval("__ob.click(document.querySelector('.key-note button'))")
    page.wait("!!document.querySelector('.modal-backdrop .wide-field input')", 30)
    password = 'ObkSys-Lab-2026-x7'  # the throwaway VM's fixture secret
    page.eval(f"__ob.setInput(document.querySelector('.modal-backdrop .wide-field input'), {json.dumps(password)})")
    page.eval("__ob.click(document.querySelector('.modal-backdrop .modal-footer .btn-primary'))")
    reading = []
    t0 = time.monotonic()
    while time.monotonic() - t0 < 180:
        st = page.eval('__ob.state()')
        if not reading or st != reading[-1]:
            reading.append(st)
        if not st.get('modal') and not st.get('key'):
            break
        if st.get('err'):
            break
        time.sleep(0.3)
    showed_loading = any(s.get('busy') or 'opening' in (s.get('opening') or '').lower() or s.get('loading')
                         for s in reading)
    note('password', states=reading, loading_shown=bool(showed_loading))
    shot('after-password')
    assert not page.eval("!!document.querySelector('.modal-backdrop p.err')"), 'the password was refused'
    assert showed_loading, 'no loading state while the copy was read'

    # 5. Target disk: the blank one.
    page.wait("__ob.all('.disk-card.clickable').length > 0", 120)
    cards = page.eval("__ob.all('.disk-card.clickable').map(c => __ob.txt(c).slice(0, 200))")
    note('targets', cards=cards)
    picked = page.eval("""(() => { const c = __ob.all('.disk-card.clickable').find(x =>
        x.querySelector('.seg.free') && x.querySelectorAll('.seg:not(.free)').length === 0);
        return __ob.click(c) ? __ob.txt(c).slice(0, 120) : null; })()""")
    assert picked, 'no blank target disk card'
    note('target-picked', card=picked)
    # 6. bootable (choosing the disk turned it off), 7. place on the free space.
    page.wait("!!document.querySelector('.boot-check input')", 30)
    page.eval("(() => { const b = document.querySelector('.boot-check input'); if (!b.checked) b.click(); })()")
    assert page.eval("document.querySelector('.boot-check input').checked"), 'bootable did not stay ticked'
    page.wait("!!document.querySelector('button.seg.free.free-pick')", 30)
    page.eval("__ob.click(document.querySelector('button.seg.free.free-pick'))")
    time.sleep(1)
    fit = page.eval("__ob.txt(document.querySelector('.fit-msg'))")
    note('placed', fit_msg=fit, instruction=page.eval("__ob.txt(document.querySelector('.conn-instr'))"))
    shot('placed')

    # 8. Restore -> plan check.
    page.eval("__ob.click(document.querySelector('.footer button.btn.green'))")
    page.wait("!!__ob.lastDialog('Restore plan check')", 60)
    page.wait("!__ob.byText('.finding-line', 'checking')", 120)
    findings = page.eval("__ob.all('.overlay .finding').map(f => f.className + ': ' + __ob.txt(f))")
    ops = page.eval("__ob.all('.overlay ol.ops li').map(__ob.txt)")
    note('plan-check', findings=findings, ops=ops)
    shot('plan-check')
    assert not any(f.startswith('finding err') for f in findings), 'the plan check found errors'
    page.wait("(() => { const b = __ob.lastDialog('Restore plan check').querySelector('.btn.danger-solid'); return b && !b.disabled; })()", 60)
    page.eval("__ob.click(__ob.lastDialog('Restore plan check').querySelector('.btn.danger-solid'))")

    # 9. Boot entries offered -> yes.
    page.wait("!!__ob.lastDialog('Restore the boot entries')", 60)
    entries = page.eval("__ob.txt(__ob.lastDialog('Restore the boot entries'))")
    note('boot-entries', dialog=entries[:3000])
    shot('boot-entries')
    page.eval("__ob.click(__ob.byText('.overlay .dialog .btn.danger-solid', 'yes'))")

    # 10. Typed confirmation.
    page.wait("!!__ob.lastDialog('Confirm restore')", 120)
    page.eval("__ob.setInput(__ob.lastDialog('Confirm restore').querySelector('input[type=text]'), 'YES')")
    page.wait("!__ob.lastDialog('Confirm restore').querySelector('.btn.danger-solid').disabled", 15)
    shot('confirm')
    page.eval("__ob.click(__ob.lastDialog('Confirm restore').querySelector('.btn.danger-solid'))")

    # 11. Progress to the end.
    page.wait("!!document.querySelector('.run .run-title')", 60)
    last = None
    t0 = time.monotonic()
    while time.monotonic() - t0 < 3600:
        title = page.eval("__ob.txt(document.querySelector('.run .run-title'))")
        rows = page.eval("__ob.all('ol.ops-run > li').map(li => li.className.replace('op ', '') + ' | ' + __ob.txt(li).replace(/\\s+/g, ' ').slice(0, 220))")
        if (title, rows) != last:
            note('progress', title=title, ops=rows)
            last = (title, rows)
        if title and 'restoring' not in title.lower():
            break
        time.sleep(5)
        budget()
    shot('finished')
    follow = page.eval("__ob.all('.run-followups .finding').map(__ob.txt)")
    title = page.eval("__ob.txt(document.querySelector('.run .run-title'))")
    rows = page.eval("__ob.all('ol.ops-run > li').map(li => li.className)")
    passed = title.lower().startswith('restore completed') and not any('failed' in r for r in rows)
    note('result', title=title, ops=rows, followups=follow, passed=passed)
    try:
        screendump('vm-final')
    except Exception:
        pass
    return 'done' if passed else 'fail'
