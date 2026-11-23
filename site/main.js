const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches
const sleep = ms => new Promise(r => setTimeout(r, ms))

const GH = '<path d="M12 .5a11.5 11.5 0 0 0-3.6 22.4c.6.1.8-.3.8-.6v-2c-3.2.7-3.9-1.5-3.9-1.5-.5-1.3-1.3-1.7-1.3-1.7-1-.7.1-.7.1-.7 1.2.1 1.8 1.2 1.8 1.2 1 1.8 2.8 1.3 3.5 1 .1-.8.4-1.3.7-1.6-2.6-.3-5.3-1.3-5.3-5.7 0-1.3.5-2.3 1.2-3.1-.1-.3-.5-1.5.1-3.1 0 0 1-.3 3.2 1.2a11 11 0 0 1 5.8 0c2.2-1.5 3.2-1.2 3.2-1.2.6 1.6.2 2.8.1 3.1.8.8 1.2 1.9 1.2 3.1 0 4.4-2.7 5.4-5.3 5.7.4.4.8 1.1.8 2.2v3.3c0 .3.2.7.8.6A11.5 11.5 0 0 0 12 .5Z"/>'
document.querySelectorAll('svg[data-gh]').forEach(s => { s.setAttribute('viewBox', '0 0 24 24'); s.setAttribute('fill', 'currentColor'); s.innerHTML = GH })

// reveal on scroll
const io = new IntersectionObserver(es => es.forEach(e => {
  if (e.isIntersecting) { e.target.classList.add('on'); io.unobserve(e.target) }
}), { threshold: .15 })
document.querySelectorAll('.rv').forEach(el => reduce ? el.classList.add('on') : io.observe(el))

// copy install commands
document.getElementById('copy').onclick = e => {
  navigator.clipboard.writeText(document.getElementById('cmds').innerText.replace(/^\$ /gm, ''))
  e.target.textContent = 'Copied'; setTimeout(() => e.target.textContent = 'Copy', 1600)
}

// hero: the disk moves laptop -> bucket -> server-2 and back
const scene = document.querySelector('.scene')
if (innerWidth < 820) {
  // pull the machines in under the bucket so the scene stays readable on a phone
  scene.setAttribute('viewBox', '0 0 600 330')
  scene.querySelectorAll('rect.card,rect.slot').forEach(e => {
    const left = +e.getAttribute('x') < 500, card = e.classList.contains('card')
    e.setAttribute('width', card ? 270 : 64)
    e.setAttribute('x', left ? (card ? 10 : 113) : (card ? 320 : 423))
  })
  scene.querySelectorAll('text').forEach(e => { const x = +e.getAttribute('x'); e.setAttribute('x', x < 400 ? 34 : x > 600 ? 344 : 300) })
  scene.querySelector('.bk').setAttribute('x', 236)
  document.getElementById('trail').setAttribute('d', 'M145 238 C145 120 240 70 300 70 C360 70 455 120 455 238')
}
const trail = document.getElementById('trail'), chip = document.getElementById('chip'), L = trail.getTotalLength()
const sl = document.getElementById('sl'), sr = document.getElementById('sr'), sb = document.getElementById('sb')
const at = u => { const p = trail.getPointAtLength(u * L); chip.setAttribute('transform', `translate(${p.x} ${p.y})`) }
const set = (el, txt, on) => { el.textContent = txt; el.classList.toggle('on', !!on) }
const move = (a, b, ms) => new Promise(done => {
  const t0 = performance.now()
  const step = t => { const k = Math.min(1, (t - t0) / ms), e = k < .5 ? 2 * k * k : 1 - Math.pow(-2 * k + 2, 2) / 2; at(a + (b - a) * e); k < 1 ? requestAnimationFrame(step) : done() }
  requestAnimationFrame(step)
})
;(async () => {
  if (reduce) { at(.5); set(sb, 'head v42', 1); set(sl, 'detached'); set(sr, 'attaching…', 1); return }
  let v = 41, from = 0, a = sl, b = sr
  at(0); set(a, 'attached', 1)
  for (;;) {
    await sleep(1600)
    set(a, 'uploading…', 1); await sleep(900)
    set(sb, 'head v' + ++v, 1); await move(from, .5, 1200); set(a, 'detached')
    await sleep(700); set(b, 'attaching…', 1)
    await move(.5, 1 - from, 1200); set(b, 'attached', 1); set(sb, 'head v' + v)
    from = 1 - from; [a, b] = [b, a]
  }
})()

// terminal: a short real session, typed
const term = document.getElementById('term')
const esc = s => s.replace(/&/g, '&amp;').replace(/</g, '&lt;')
const SESSION = [
  ['sudo mica disk create photos --size 1T --store s3.toml', [
    '<span class="ok">✔</span> Made disk photos: 1T in chunks of 1M']],
  ['sudo mica disk attach photos', [
    '<span class="ok">✔</span> Attached disk photos as <span class="acc">/dev/mica/photos</span>']],
  ['sudo mica status', [
    '<span class="dim">DISK    DEVICE            WRITE   UPLOAD  UNSAVED    CACHE        LAST SAVE  STATE</span>',
    'photos  /dev/mica/photos  160M/s  91M/s   2.4G / 4G  512M / 512M  47 s ago   <span class="ok">ok</span>']],
  ['sudo mica disk detach photos', [
    '<span class="ok">✔</span> Detached disk photos. Any machine can attach it now']],
]
const PROMPT = '<span class="p">$ </span>'
;(async () => {
  const full = SESSION.map(([c, out]) => PROMPT + esc(c) + '\n' + out.join('\n')).join('\n\n')
  if (reduce) { term.innerHTML = full; return }
  await new Promise(r => { const o = new IntersectionObserver(es => { if (es[0].isIntersecting) { o.disconnect(); r() } }); o.observe(term) })
  for (;;) {
    let done = ''
    for (const [cmd, out] of SESSION) {
      for (let i = 0; i <= cmd.length; i++) {
        term.innerHTML = done + PROMPT + esc(cmd.slice(0, i)) + '<span class="cur"></span>'
        await sleep(i ? 28 + Math.random() * 30 : 500)
      }
      await sleep(350)
      done += PROMPT + esc(cmd) + '\n' + out.join('\n') + '\n\n'
      term.innerHTML = done + PROMPT + '<span class="cur"></span>'
      await sleep(1300)
    }
    await sleep(2600)
  }
})()

// hero: writes land on the SSD, then the changed chunks go up to the bucket
;(async () => {
  const N = 24, g1 = document.getElementById('g1'), g2 = document.getElementById('g2')
  g1.innerHTML = g2.innerHTML = '<i></i>'.repeat(N)
  const c1 = [...g1.children], c2 = [...g2.children]
  const st0 = document.getElementById('st0'), st1 = document.getElementById('st1'), st2 = document.getElementById('st2')
  const l1 = document.getElementById('l1'), l2 = document.getElementById('l2')
  ;[1, 2, 4, 9, 10, 13, 17, 20].forEach(k => { c1[k].className = 'saved'; c2[k].className = 'saved' })
  const shoot = link => { const i = document.createElement('i'); link.append(i); setTimeout(() => i.remove(), 720) }
  const unsaved = () => c1.filter(c => c.className === 'dirty').length
  const show = () => set(st1, unsaved() + ' unsaved', unsaved() > 0)
  if (reduce) {
    ;[3, 6, 11, 18].forEach(k => c1[k].className = 'dirty'); show()
    set(st0, 'writing', 1); return
  }
  let v = 41
  for (;;) {
    set(st0, 'writing', 1)
    for (let n = 0; n < 6; n++) {
      const k = Math.floor(Math.random() * N)
      shoot(l1); setTimeout(() => { c1[k].className = 'dirty'; show() }, 700)
      await sleep(520)
    }
    await sleep(900); set(st0, 'idle')
    await sleep(500); set(st1, 'uploading…', 1)
    const d = c1.map((c, k) => c.className === 'dirty' ? k : -1).filter(k => k >= 0)
    for (const k of d) {
      shoot(l2)
      setTimeout(() => { c1[k].className = 'saved'; c2[k].className = 'new'; setTimeout(() => c2[k].className = 'saved', 900) }, 700)
      await sleep(260)
    }
    await sleep(800); set(st2, 'head v' + ++v, 1); show()
    await sleep(1600); set(st2, 'head v' + v)
    await sleep(600)
  }
})()
