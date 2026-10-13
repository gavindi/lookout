// The reading pane's conversation script (item 1.7). It runs as a trusted
// user script in its own world, injected into every frame of the
// conversation document; the page itself never runs script (see
// `conversation_view.rs`).
(() => {
  // A message body frame: sandboxed without `allow-scripts`, so no click
  // listener could run here - and the document's `frame-src 'none'` would
  // silently drop an in-frame navigation anyway. Retarget every link to a
  // new window instead: the click then reaches the app as a new-window
  // decision, which opens it in the browser.
  if (window !== window.top) {
    for (const link of document.querySelectorAll('a[href], area[href]')) {
      link.setAttribute('target', '_blank');
    }
    return;
  }

  const post = (message) => window.webkit.messageHandlers.lookout.postMessage(JSON.stringify(message));

  // Size a body frame to its content, and keep it sized as the content
  // reflows (images arriving, the pane resizing).
  const watchFrame = (frame) => {
    const fit = () => {
      const doc = frame.contentDocument;
      if (!doc || !doc.documentElement) return;
      const height = Math.ceil(doc.body ? doc.body.scrollHeight : doc.documentElement.scrollHeight);
      if (height > 0 && frame.style.height !== `${height}px`) frame.style.height = `${height}px`;
    };
    const attach = () => {
      const doc = frame.contentDocument;
      if (!doc || !doc.documentElement) return;
      fit();
      const observer = new ResizeObserver(fit);
      observer.observe(doc.documentElement);
      if (doc.body) observer.observe(doc.body);
    };
    frame.addEventListener('load', attach);
    if (frame.contentDocument && frame.contentDocument.readyState === 'complete') attach();
  };

  const cardIndex = (card) => Number(card.dataset.card);

  const setExpanded = (card, expanded, notify) => {
    if (card.classList.contains('expanded') === expanded) return;
    card.classList.toggle('expanded', expanded);
    const head = card.querySelector('.head');
    if (head) head.setAttribute('aria-expanded', String(expanded));
    if (notify) post({ action: expanded ? 'expand' : 'collapse', card: cardIndex(card) });
  };

  document.addEventListener('click', (event) => {
    const target = event.target.closest('[data-action]');
    if (!target) return;
    const action = target.dataset.action;
    if (action === 'expand-all' || action === 'collapse-all') {
      for (const card of document.querySelectorAll('.card')) setExpanded(card, action === 'expand-all', true);
      return;
    }
    const card = target.closest('[data-card]');
    if (!card) return;
    event.preventDefault();
    if (action === 'toggle') {
      setExpanded(card, !card.classList.contains('expanded'), true);
      return;
    }
    const rect = target.getBoundingClientRect();
    post({ action, card: cardIndex(card), part: target.dataset.part, x: rect.left, y: rect.bottom });
  });

  document.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    const head = event.target.closest('.head');
    if (!head) return;
    event.preventDefault();
    const card = head.closest('[data-card]');
    setExpanded(card, !card.classList.contains('expanded'), true);
  });

  // Load each body: with markup JavaScript off WebKit's parser drops a
  // `srcdoc` attribute, so the app ships it as `data-srcdoc` and it is set
  // here as the property, which WebKit honours.
  for (const frame of document.querySelectorAll('iframe.body')) {
    watchFrame(frame);
    const srcdoc = frame.dataset.srcdoc;
    if (srcdoc !== undefined) {
      frame.removeAttribute('data-srcdoc');
      frame.srcdoc = srcdoc;
    }
  }

  // The app's calls (`evaluate_javascript` in this world).
  window.lookout = {
    // A card's body arrived: replace its "Loading…" slot (or an earlier
    // frame) with a sized body frame.
    setBody(index, srcdoc, extras) {
      const card = document.getElementById(`card-${index}`);
      if (!card) return false;
      // `extras` is `card_extras_html`'s output: the app's own markup.
      const holder = card.querySelector('.extras');
      if (holder) holder.innerHTML = extras;
      const slot = card.querySelector('iframe.body, .loading');
      if (!slot) return false;
      const frame = document.createElement('iframe');
      frame.className = 'body';
      frame.title = 'Message body';
      frame.setAttribute('sandbox', 'allow-same-origin allow-popups');
      frame.setAttribute('scrolling', 'no');
      slot.replaceWith(frame);
      watchFrame(frame);
      frame.srcdoc = srcdoc;
      return true;
    },
    // A card's state changed (read, flag, category): swap its header and
    // notes in place, leaving the loaded body frame alone.
    updateCard(index, html) {
      const card = document.getElementById(`card-${index}`);
      if (!card) return false;
      // `html` is `card_html`'s output: the app's own markup, every
      // message-derived string escaped, and no script can run here anyway.
      const holder = document.createElement('div');
      holder.innerHTML = html;
      const fresh = holder.firstElementChild;
      if (!fresh) return false;
      const expanded = card.classList.contains('expanded');
      card.className = fresh.className;
      card.classList.toggle('expanded', expanded);
      const head = fresh.querySelector('.head');
      if (head) {
        head.setAttribute('aria-expanded', String(expanded));
        card.querySelector('.head').replaceWith(head);
      }
      return true;
    },
    bodyFailed(index) {
      const card = document.getElementById(`card-${index}`);
      const slot = card && card.querySelector('.loading');
      if (slot) slot.textContent = "This message couldn't be loaded.";
    },
    setExpanded(index, expanded) {
      const card = document.getElementById(`card-${index}`);
      if (card) setExpanded(card, expanded, false);
    },
    scrollTo(index) {
      const card = document.getElementById(`card-${index}`);
      if (card) card.scrollIntoView({ block: 'start' });
    },
    restoreScroll(y) {
      window.scrollTo(0, y);
    },
  };
})();
