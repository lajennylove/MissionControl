/*!
 * Mission Control's featured image box on the post editor's sidebar. GPL-2.0-or-later.
 *
 * WordPress's Featured image meta box (`post_thumbnail_meta_box` and `_wp_post_thumbnail_html`)
 * and its media frame (`wp.media.featuredImage`): `editor.js` loads this script when the form
 * has the `featured_media` field (Ignite's Image field, a number field holding the media id).
 *
 * - The box: "Set featured image" when there is none; otherwise the image (with its alternative
 *   text), "Click the image to edit or update" and "Remove featured image".
 * - "Set featured image" (or the image) opens the modal "Featured image" with two tabs, in this
 *   order: "Uploaded to this post" (the images attached to the post, shown first) and "Media
 *   Library" (every image, with a search). Both load 40 items at a time with "Load more".
 * - Selecting an item and pressing the modal's "Set featured image" writes its id in the
 *   field, which the form saves as before (the server still refuses an id that is not a media
 *   item); "Remove featured image" empties it.
 *
 * Nothing changes until the user does, so the panel's unsaved changes alert stays quiet on an
 * unchanged form. Without this script the field shows as Ignite renders it. Every value from
 * the server is set as text or as an attribute, never as HTML.
 */
(function () {
  'use strict';

  var api = window.mcEditor = window.mcEditor || {};
  if (api.featuredImage) {
    return;
  }
  api.featuredImage = true;

  /** Where the admin is mounted. */
  var ADMIN = '/mc-admin';

  /** How many items a page holds (WordPress's media modal asks for 40). */
  var PER_PAGE = 40;

  /** The field's name: the REST API's `featured_media`. */
  var FIELD = 'featured_media';

  function el(tag, attributes, text) {
    var node = document.createElement(tag);
    Object.keys(attributes || {}).forEach(function (name) {
      node.setAttribute(name, attributes[name]);
    });
    if (text !== undefined && text !== null) {
      node.textContent = String(text);
    }
    return node;
  }

  /** Tells the form (and the panel's alert) that `element`'s value changed. */
  function changed(element) {
    element.dispatchEvent(new Event('input', { bubbles: true }));
    element.dispatchEvent(new Event('change', { bubbles: true }));
  }

  /** The id of the post being edited (`/mc-admin/<type>/<id>/edit`, or the form shown again). */
  function editedPost() {
    var match = /^\/mc-admin\/[a-z0-9_-]+\/(\d+)(?:\/edit)?\/?$/.exec(window.location.pathname);
    return match ? match[1] : '';
  }

  /** A whole number above 0, or 0. */
  function positive(value) {
    var n = parseInt(value, 10);
    return n > 0 ? n : 0;
  }

  /** The URL an item shows at `preferred` size (falling back to smaller ones, then the file). */
  function imageUrl(item, preferred) {
    var sizes = (item && item.sizes) || {};
    var order = preferred === 'medium'
      ? ['medium', 'large', 'full', 'thumbnail']
      : ['thumbnail', 'medium', 'full'];
    for (var i = 0; i < order.length; i++) {
      if (sizes[order[i]] && sizes[order[i]].url) {
        return sizes[order[i]].url;
      }
    }
    return item && item.url ? item.url : '';
  }

  /** Reads an `admin-ajax.php` answer: `{ data, total }`, or an error with its message. */
  function readJson(response) {
    var type = response.headers.get('Content-Type') || '';
    if (type.indexOf('application/json') === -1) {
      throw new Error(response.status === 200 || response.redirected
        ? 'Your session has expired. Please sign in again.'
        : 'The media library could not be reached (' + response.status + ').');
    }
    return response.json().then(function (body) {
      if (!body || body.success !== true) {
        var message = body && body.data && body.data.message;
        throw new Error(message || 'The media library could not be read.');
      }
      return { data: body.data, total: positive(response.headers.get('X-MC-Total')) };
    });
  }

  // -------------------------------------------------------------------------------------------
  // One tab of the modal: a list of images that loads 40 at a time.

  function Browser(frame, options) {
    this.frame = frame;
    this.uploadedTo = options.uploadedTo || '';
    this.onlyAttached = Boolean(options.onlyAttached);
    this.page = 0;
    this.total = 0;
    this.count = 0;
    this.search = '';
    this.ticket = 0;
    this.started = false;
    this.panel = el('div', {
      'class': 'mc-featured-browser',
      id: options.id,
      role: 'tabpanel',
      'aria-labelledby': options.tabId
    });
    if (options.searchable) {
      var self = this;
      var searchId = options.id + '-search';
      this.panel.appendChild(el('label', { 'for': searchId }, 'Search media'));
      this.searchInput = el('input', {
        type: 'search', id: searchId, 'class': 'ig-input', maxlength: '200'
      });
      var timer = null;
      var searchNow = function () {
        window.clearTimeout(timer);
        var typed = self.searchInput.value.trim();
        if (typed !== self.search) {
          self.search = typed;
          self.reload();
        }
      };
      this.searchInput.addEventListener('input', function () {
        window.clearTimeout(timer);
        timer = window.setTimeout(searchNow, 300);
      });
      this.searchInput.addEventListener('keydown', function (event) {
        if (event.key === 'Enter') {
          event.preventDefault();
          searchNow();
        }
      });
      this.panel.appendChild(this.searchInput);
    }
    this.list = el('ul', { 'class': 'mc-media-items', 'aria-label': options.label });
    this.status = el('p', { 'class': 'mc-media-status', role: 'status', 'aria-live': 'polite' });
    this.counter = el('p', { 'class': 'mc-featured-count' });
    this.more = el('button', { type: 'button', 'class': 'ig-btn ig-btn-outline' }, 'Load more');
    this.more.hidden = true;
    var browser = this;
    this.more.addEventListener('click', function () {
      browser.load(true);
    });
    this.panel.appendChild(this.list);
    this.panel.appendChild(this.status);
    this.panel.appendChild(this.counter);
    this.panel.appendChild(this.more);
  }

  Browser.prototype.start = function () {
    if (!this.started) {
      this.started = true;
      this.load(false);
    }
  };

  Browser.prototype.reload = function () {
    this.page = 0;
    this.total = 0;
    this.count = 0;
    this.list.textContent = '';
    this.counter.textContent = '';
    this.load(false);
  };

  /** Loads the next page; `focusFirst` moves the focus to its first item ("Load more"). */
  Browser.prototype.load = function (focusFirst) {
    var self = this;
    if (this.onlyAttached && !this.uploadedTo) {
      // A post not saved yet has nothing uploaded to it.
      this.status.textContent = 'No media items found.';
      return;
    }
    var query = 'action=query-attachments&type=image&page=' + (this.page + 1) +
      '&per_page=' + PER_PAGE;
    if (this.uploadedTo) {
      query += '&uploaded_to=' + encodeURIComponent(this.uploadedTo);
    }
    if (this.search) {
      query += '&search=' + encodeURIComponent(this.search);
    }
    var ticket = ++this.ticket;
    this.status.textContent = 'Loading…';
    this.more.hidden = true;
    window.fetch(ADMIN + '/admin-ajax.php?' + query, {
      credentials: 'same-origin',
      headers: { Accept: 'application/json' }
    }).then(readJson).then(function (answer) {
      if (ticket !== self.ticket) {
        return;
      }
      self.page += 1;
      self.total = answer.total;
      var first = null;
      (answer.data || []).forEach(function (item) {
        var button = self.add(item);
        first = first || button;
      });
      self.more.hidden = self.count >= self.total;
      self.status.textContent = self.count ? '' : 'No media items found.';
      self.counter.textContent = self.count
        ? 'Showing ' + self.count + ' of ' + Math.max(self.total, self.count) + ' media items'
        : '';
      if (focusFirst && first) {
        first.focus();
      }
    }).catch(function (error) {
      if (ticket === self.ticket) {
        self.status.textContent = error.message;
        self.more.hidden = self.page === 0;
      }
    });
  };

  Browser.prototype.add = function (item) {
    var frame = this.frame;
    if (!item || !positive(item.id)) {
      return null;
    }
    this.count += 1;
    var entry = el('li');
    var button = el('button', {
      type: 'button',
      'class': 'mc-media-item',
      'aria-pressed': String(frame.selected !== null && positive(frame.selected.id) === positive(item.id)),
      'data-id': String(positive(item.id))
    });
    var src = imageUrl(item, 'thumbnail');
    if (src) {
      button.appendChild(el('img', { src: src, alt: item.alt || '', loading: 'lazy' }));
    }
    button.appendChild(el('span', { 'class': 'mc-media-name' }, item.title || item.filename || ''));
    button.addEventListener('click', function () {
      frame.select(item);
    });
    button.addEventListener('dblclick', function () {
      frame.select(item);
      frame.confirm();
    });
    entry.appendChild(button);
    this.list.appendChild(entry);
    return button;
  };

  // -------------------------------------------------------------------------------------------
  // The modal: WordPress's featured image frame.

  function Frame(box) {
    this.box = box;
    this.selected = null;
    this.build();
  }

  Frame.prototype.build = function () {
    var self = this;
    var dialog = el('dialog', {
      'class': 'mc-media-dialog mc-featured-dialog',
      'aria-labelledby': 'mc-featured-title'
    });
    var header = el('div', { 'class': 'mc-media-header' });
    header.appendChild(el('h2', { id: 'mc-featured-title' }, 'Featured image'));
    var close = el('button', {
      type: 'button', 'class': 'mc-media-close', 'aria-label': 'Close dialog'
    }, '×');
    close.addEventListener('click', function () {
      dialog.close();
    });
    header.appendChild(close);
    dialog.appendChild(header);

    var post = editedPost();
    this.browsers = [
      new Browser(this, {
        id: 'mc-featured-attached',
        tabId: 'mc-featured-tab-attached',
        label: 'Uploaded to this post',
        uploadedTo: post,
        onlyAttached: true,
        searchable: false
      }),
      new Browser(this, {
        id: 'mc-featured-library',
        tabId: 'mc-featured-tab-library',
        label: 'Media Library',
        searchable: true
      })
    ];
    var tabs = el('div', { 'class': 'mc-media-tabs', role: 'tablist', 'aria-label': 'Featured image' });
    this.tabs = [
      el('button', {
        type: 'button', role: 'tab', id: 'mc-featured-tab-attached',
        'aria-controls': 'mc-featured-attached'
      }, 'Uploaded to this post'),
      el('button', {
        type: 'button', role: 'tab', id: 'mc-featured-tab-library',
        'aria-controls': 'mc-featured-library'
      }, 'Media Library')
    ];
    this.tabs.forEach(function (tab, index) {
      tab.addEventListener('click', function () {
        self.show(index);
      });
      tab.addEventListener('keydown', function (event) {
        if (event.key === 'ArrowRight' || event.key === 'ArrowLeft') {
          event.preventDefault();
          var next = (index + (event.key === 'ArrowRight' ? 1 : -1) + self.tabs.length) %
            self.tabs.length;
          self.show(next);
          self.tabs[next].focus();
        }
      });
      tabs.appendChild(tab);
    });
    dialog.appendChild(tabs);

    var body = el('div', { 'class': 'mc-media-library' });
    var panels = el('div', { 'class': 'mc-media-browser' });
    this.browsers.forEach(function (browser) {
      panels.appendChild(browser.panel);
    });
    body.appendChild(panels);
    this.details = el('div', { 'class': 'mc-media-details', 'aria-label': 'Attachment details', role: 'region' });
    body.appendChild(this.details);
    dialog.appendChild(body);

    var toolbar = el('div', { 'class': 'mc-featured-toolbar' });
    this.button = el('button', { type: 'button', 'class': 'ig-btn ig-btn-primary' }, 'Set featured image');
    this.button.disabled = true;
    this.button.addEventListener('click', function () {
      self.confirm();
    });
    toolbar.appendChild(this.button);
    dialog.appendChild(toolbar);

    dialog.addEventListener('close', function () {
      dialog.remove();
      self.box.focusTrigger();
    });
    this.dialog = dialog;
    this.renderDetails();
  };

  /** Shows tab `index` ("Uploaded to this post" is 0, "Media Library" 1). */
  Frame.prototype.show = function (index) {
    var self = this;
    this.tabs.forEach(function (tab, i) {
      tab.setAttribute('aria-selected', String(i === index));
      tab.tabIndex = i === index ? 0 : -1;
      self.browsers[i].panel.hidden = i !== index;
    });
    this.browsers[index].start();
  };

  Frame.prototype.open = function () {
    document.body.appendChild(this.dialog);
    this.dialog.showModal();
    this.show(0);
    this.tabs[0].focus();
  };

  Frame.prototype.select = function (item) {
    this.selected = item;
    var id = String(positive(item.id));
    Array.prototype.forEach.call(this.dialog.querySelectorAll('.mc-media-item'), function (b) {
      b.setAttribute('aria-pressed', String(b.getAttribute('data-id') === id));
    });
    this.button.disabled = false;
    this.renderDetails();
  };

  /** WordPress's attachment details beside the list: the selected image and its facts. */
  Frame.prototype.renderDetails = function () {
    var details = this.details;
    var item = this.selected;
    details.textContent = '';
    if (!item) {
      details.appendChild(el('p', {}, 'Select an image.'));
      return;
    }
    details.appendChild(el('h3', {}, 'Attachment details'));
    var src = imageUrl(item, 'medium');
    if (src) {
      details.appendChild(el('img', { 'class': 'mc-featured-preview', src: src, alt: item.alt || '' }));
    }
    var facts = el('dl', { 'class': 'mc-grid-facts' });
    var fact = function (label, value) {
      if (value === undefined || value === null || value === '') {
        return;
      }
      facts.appendChild(el('dt', {}, label));
      facts.appendChild(el('dd', {}, value));
    };
    fact('File name', item.filename);
    fact('Uploaded on', item.dateFormatted);
    fact('File size', item.filesizeHumanReadable);
    if (positive(item.width) && positive(item.height)) {
      fact('Dimensions', positive(item.width) + ' by ' + positive(item.height) + ' pixels');
    }
    fact('Alternative Text', item.alt);
    details.appendChild(facts);
  };

  Frame.prototype.confirm = function () {
    if (!this.selected) {
      return;
    }
    this.box.set(this.selected);
    this.dialog.close();
  };

  // -------------------------------------------------------------------------------------------
  // The box on the sidebar.

  function Box(field, input) {
    var self = this;
    this.field = field;
    this.input = input;
    this.current = null;
    // Ignite's field: the number field (kept, out of sight, as the form's value), the stored
    // item and the link to the library (replaced by the box). Its errors stay shown.
    input.hidden = true;
    Array.prototype.forEach.call(field.children, function (child) {
      if (child.tagName === 'P' && !child.classList.contains('text-error-emphasis')) {
        child.hidden = true;
      }
    });
    var label = field.querySelector('label[for="' + FIELD + '"]');
    this.box = el('div', { 'class': 'mc-featured', role: 'group' });
    if (label) {
      label.id = label.id || 'mc-featured-label';
      label.removeAttribute('for');
      this.box.setAttribute('aria-labelledby', label.id);
    }
    input.parentNode.insertBefore(this.box, input.nextSibling);
    var id = positive(input.value);
    if (id) {
      // What Ignite showed until the item is read: its address and title.
      var shown = field.querySelector('p img');
      this.current = {
        id: id,
        url: shown ? shown.getAttribute('src') : '',
        alt: '',
        title: shown ? shown.getAttribute('alt') : ''
      };
      window.fetch(ADMIN + '/admin-ajax.php?action=get-attachment&id=' + id, {
        credentials: 'same-origin',
        headers: { Accept: 'application/json' }
      }).then(readJson).then(function (answer) {
        if (self.current && positive(self.current.id) === id) {
          self.current = answer.data;
          self.render();
        }
      }).catch(function () {
        // The address Ignite gave stays shown.
      });
    }
    this.render();
  }

  Box.prototype.render = function () {
    var self = this;
    var box = this.box;
    box.textContent = '';
    var item = this.current;
    if (!item) {
      var set = el('button', {
        type: 'button', id: 'set-post-thumbnail', 'class': 'ig-btn ig-btn-outline'
      }, 'Set featured image');
      set.addEventListener('click', function () {
        self.open();
      });
      var wrap = el('p');
      wrap.appendChild(set);
      box.appendChild(wrap);
      this.trigger = set;
      return;
    }
    var image = el('button', {
      type: 'button',
      id: 'set-post-thumbnail',
      'class': 'mc-featured-image',
      'aria-describedby': 'set-post-thumbnail-desc'
    });
    var src = imageUrl(item, 'medium');
    if (src) {
      image.appendChild(el('img', { src: src, alt: item.alt || '' }));
    }
    if (!item.alt) {
      // The button's name, when the image has no alternative text.
      image.appendChild(el('span', { 'class': 'mc-visually-hidden' },
        item.title || 'Featured image'));
    }
    image.addEventListener('click', function () {
      self.open();
    });
    var first = el('p');
    first.appendChild(image);
    box.appendChild(first);
    box.appendChild(el('p', { id: 'set-post-thumbnail-desc', 'class': 'mc-featured-howto' },
      'Click the image to edit or update'));
    var remove = el('a', { href: '#', id: 'remove-post-thumbnail', role: 'button' },
      'Remove featured image');
    remove.addEventListener('click', function (event) {
      event.preventDefault();
      self.set(null);
    });
    var last = el('p');
    last.appendChild(remove);
    box.appendChild(last);
    this.trigger = image;
  };

  Box.prototype.open = function () {
    new Frame(this).open();
  };

  /** Stores `item`'s id in the field (or empties it), and shows the box again. */
  Box.prototype.set = function (item) {
    var value = item ? String(positive(item.id)) : '';
    this.current = item;
    if (this.input.value !== value) {
      this.input.value = value;
      changed(this.input);
    }
    this.render();
    this.focusTrigger();
  };

  Box.prototype.focusTrigger = function () {
    if (this.trigger && document.contains(this.trigger)) {
      this.trigger.focus();
    }
  };

  function start() {
    var field = document.querySelector('[data-ignite-field="' + FIELD + '"]');
    var input = field && field.querySelector('input[name="' + FIELD + '"][data-ignite-media="image"]');
    if (input && !field.querySelector('.mc-featured')) {
      api.featuredBox = new Box(field, input);
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
}());
