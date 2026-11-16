/*!
 * Mission Control's Add Media picker (Task 6.4). GPL-2.0-or-later.
 *
 * `editor.js` loads this script beside the editor. It fills `window.mcEditor.media.open`, which
 * the Add Media button calls with the editor's id, with WordPress's media modal in small:
 *
 * - Upload Files: one file, sent to `/mc-admin/async-upload.php` with the form's CSRF token
 *   (the `X-CSRF-TOKEN` header) and attached to the post being edited; the new item is then
 *   selected in the library.
 * - Media Library: the site's attachments from `/mc-admin/admin-ajax.php?action=
 *   query-attachments`, newest first, searchable, 40 at a time.
 * - Attachment details: for an image, its alternative text, caption, size and alignment.
 * - Insert into post: WordPress's markup, through `window.mcEditor.insert`: an image as
 *   `<img src alt width height class="alignnone size-medium wp-image-ID" />`, any other file as
 *   a link to it. An image with a caption is wrapped in WordPress's `[caption id="attachment_ID"
 *   align="alignX" width="W"]<img …> Caption[/caption]`.
 *
 * Every element is built with the DOM, and the server's values are set as text or attributes,
 * never as HTML. The inserted markup escapes them.
 */
(function () {
  'use strict';

  var api = window.mcEditor = window.mcEditor || {};

  /** Where the admin is mounted. */
  var ADMIN = '/mc-admin';

  /** How many items a page of the library holds (WordPress's media modal asks for 40). */
  var PER_PAGE = 40;

  /** The image sizes offered, in WordPress's order (`image_size_names_choose`). */
  var SIZES = [
    ['thumbnail', 'Thumbnail'],
    ['medium', 'Medium'],
    ['large', 'Large'],
    ['full', 'Full Size']
  ];

  var ALIGNMENTS = [
    ['none', 'None'],
    ['left', 'Left'],
    ['center', 'Center'],
    ['right', 'Right']
  ];

  function escapeAttribute(text) {
    return String(text)
      .replace(/&/g, '&amp;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#039;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }

  function escapeHtml(text) {
    return String(text)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }

  /** A whole number of pixels, or 0. */
  function pixels(value) {
    var n = parseInt(value, 10);
    return n > 0 ? n : 0;
  }

  /**
   * An element: `attributes` are set with `setAttribute` (and `text` as its text), so nothing
   * is ever parsed as HTML.
   */
  function element(tag, attributes, text) {
    var node = document.createElement(tag);
    Object.keys(attributes || {}).forEach(function (name) {
      node.setAttribute(name, attributes[name]);
    });
    if (text !== undefined && text !== null) {
      node.textContent = String(text);
    }
    return node;
  }

  /** The CSRF token of the page's form (Ignite writes it as `_token`). */
  function csrfToken() {
    var input = document.querySelector('input[name="_token"]');
    return input ? input.value : '';
  }

  /** The id of the post being edited (`/mc-admin/<type>/<id>/edit`), or ''. */
  function editedPost() {
    var match = /^\/mc-admin\/[a-z0-9_-]+\/(\d+)\/edit\/?$/.exec(window.location.pathname);
    return match ? match[1] : '';
  }

  /** Reads a JSON answer; a sign-in page or another error becomes a message. */
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
        throw new Error(message || 'The request failed (' + response.status + ').');
      }
      return { data: body.data, total: pixels(response.headers.get('X-MC-Total')) };
    });
  }

  /**
   * A caption's text for the `[caption]` shortcode: escaped as HTML text, with `[` and `]` as
   * character references so it cannot end the shortcode, and Windows line breaks as `\n`.
   */
  function captionText(text) {
    return escapeHtml(String(text).replace(/\r\n?/g, '\n'))
      .replace(/\[/g, '&#91;')
      .replace(/\]/g, '&#93;');
  }

  /**
   * WordPress's markup for `item`: an image (`get_image_tag`) in `size`, with `alt` and the
   * `align` class, wrapped in `[caption]` when it has a `caption` and a known width
   * (`image_add_caption`: the alignment moves to the shortcode), or a link to any other file
   * (`media_send_to_editor`). The caption is inserted as escaped text.
   */
  function markup(item, size, alt, align, caption) {
    var url = String(item.url || '');
    if (item.type === 'image') {
      var chosen = item.sizes && (item.sizes[size] || item.sizes.full);
      var name = item.sizes && item.sizes[size] ? size : 'full';
      var width = pixels(chosen ? chosen.width : item.width);
      var height = pixels(chosen ? chosen.height : item.height);
      // `image_add_caption` needs a width: without one the image is inserted without the
      // shortcode, which would show nothing but the image anyway.
      var captioned = Boolean(caption && String(caption).trim() && width);
      var html = '<img src="' + escapeAttribute(chosen ? chosen.url : url) + '" alt="' +
        escapeAttribute(alt) + '"';
      if (width && height) {
        html += ' width="' + width + '" height="' + height + '"';
      }
      var classes = (captioned ? '' : 'align' + escapeAttribute(align) + ' ') +
        'size-' + escapeAttribute(name) + ' wp-image-' + pixels(item.id);
      html += ' class="' + classes + '" />';
      if (!captioned) {
        return html;
      }
      return '[caption id="attachment_' + pixels(item.id) + '" align="align' +
        escapeAttribute(align || 'none') + '" width="' + width + '"]' + html + ' ' +
        captionText(caption) + '[/caption]';
    }
    return '<a href="' + escapeAttribute(url) + '">' +
      escapeHtml(item.title || item.filename || url) + '</a>';
  }

  /** The modal for one editor. */
  function Picker(editorId) {
    this.editorId = editorId;
    this.items = [];
    this.buttons = {};
    this.selected = null;
    this.page = 0;
    this.total = 0;
    this.search = '';
    this.loading = 0;
    this.build();
  }

  Picker.prototype.build = function () {
    var self = this;
    var dialog = element('dialog', {
      'class': 'mc-media-dialog',
      'aria-labelledby': 'mc-media-title'
    });
    var header = element('div', { 'class': 'mc-media-header' });
    header.appendChild(element('h2', { id: 'mc-media-title' }, 'Add Media'));
    var close = element('button', {
      type: 'button',
      'class': 'mc-media-close',
      'aria-label': 'Close dialog'
    }, '×');
    close.addEventListener('click', function () {
      dialog.close();
    });
    header.appendChild(close);
    dialog.appendChild(header);

    var tabs = element('div', { 'class': 'mc-media-tabs', role: 'tablist' });
    this.uploadTab = element('button', {
      type: 'button', role: 'tab', id: 'mc-media-tab-upload', 'aria-controls': 'mc-media-upload'
    }, 'Upload Files');
    this.libraryTab = element('button', {
      type: 'button', role: 'tab', id: 'mc-media-tab-library', 'aria-controls': 'mc-media-library'
    }, 'Media Library');
    this.uploadTab.addEventListener('click', function () {
      self.show('upload');
    });
    this.libraryTab.addEventListener('click', function () {
      self.show('library');
    });
    tabs.appendChild(this.uploadTab);
    tabs.appendChild(this.libraryTab);
    dialog.appendChild(tabs);

    // Upload Files.
    var upload = element('form', {
      'class': 'mc-media-upload',
      id: 'mc-media-upload',
      role: 'tabpanel',
      'aria-labelledby': 'mc-media-tab-upload'
    });
    var fileLabel = element('label', { 'for': 'mc-media-file' }, 'Select a file to upload');
    this.fileInput = element('input', { type: 'file', id: 'mc-media-file', name: 'async-upload' });
    this.fileInput.required = true;
    var send = element('button', { type: 'submit', 'class': 'ig-btn ig-btn-primary' }, 'Upload');
    upload.appendChild(fileLabel);
    upload.appendChild(this.fileInput);
    upload.appendChild(send);
    upload.addEventListener('submit', function (event) {
      event.preventDefault();
      self.upload();
    });
    this.uploadPanel = upload;
    dialog.appendChild(upload);

    // Media Library: the search and the items, beside the details of the selected one.
    var library = element('div', {
      'class': 'mc-media-library',
      id: 'mc-media-library',
      role: 'tabpanel',
      'aria-labelledby': 'mc-media-tab-library'
    });
    var browser = element('div', { 'class': 'mc-media-browser' });
    var searchLabel = element('label', { 'for': 'mc-media-search' }, 'Search media');
    this.searchInput = element('input', {
      type: 'search', id: 'mc-media-search', 'class': 'ig-input', maxlength: '200'
    });
    var timer = null;
    this.searchInput.addEventListener('input', function () {
      window.clearTimeout(timer);
      timer = window.setTimeout(function () {
        self.search = self.searchInput.value.trim();
        self.reload();
      }, 300);
    });
    this.list = element('ul', { 'class': 'mc-media-items', 'aria-label': 'Media items' });
    this.more = element('button', { type: 'button', 'class': 'ig-btn ig-btn-outline' },
      'Load more');
    this.more.hidden = true;
    this.more.addEventListener('click', function () {
      self.load();
    });
    browser.appendChild(searchLabel);
    browser.appendChild(this.searchInput);
    browser.appendChild(this.list);
    browser.appendChild(this.more);
    library.appendChild(browser);

    this.details = element('form', { 'class': 'mc-media-details', 'aria-label': 'Attachment details' });
    this.details.addEventListener('submit', function (event) {
      event.preventDefault();
      self.insert();
    });
    library.appendChild(this.details);
    this.libraryPanel = library;
    dialog.appendChild(library);

    this.status = element('p', { 'class': 'mc-media-status', role: 'status', 'aria-live': 'polite' });
    dialog.appendChild(this.status);

    dialog.addEventListener('close', function () {
      dialog.remove();
    });
    this.dialog = dialog;
    this.renderDetails();
  };

  Picker.prototype.say = function (message, isError) {
    this.status.textContent = message || '';
    this.status.classList.toggle('mc-media-error', Boolean(isError));
  };

  Picker.prototype.show = function (panel) {
    var upload = panel === 'upload';
    this.uploadTab.setAttribute('aria-selected', String(upload));
    this.libraryTab.setAttribute('aria-selected', String(!upload));
    this.uploadPanel.hidden = !upload;
    this.libraryPanel.hidden = upload;
    if (!upload && !this.page && !this.loading) {
      this.load();
    }
  };

  Picker.prototype.open = function () {
    document.body.appendChild(this.dialog);
    this.dialog.showModal();
    this.show('library');
  };

  /** Starts the list again (a new search). */
  Picker.prototype.reload = function () {
    this.items = [];
    this.buttons = {};
    this.page = 0;
    this.total = 0;
    this.list.textContent = '';
    this.load();
  };

  /** Loads the next page of the library. */
  Picker.prototype.load = function () {
    var self = this;
    var query = 'action=query-attachments&page=' + (this.page + 1) + '&per_page=' + PER_PAGE;
    if (this.search) {
      query += '&search=' + encodeURIComponent(this.search);
    }
    var ticket = ++this.loading;
    this.say('Loading…');
    window.fetch(ADMIN + '/admin-ajax.php?' + query, {
      credentials: 'same-origin',
      headers: { Accept: 'application/json' }
    }).then(readJson).then(function (answer) {
      if (ticket !== self.loading) {
        return;
      }
      self.page += 1;
      self.total = answer.total;
      (answer.data || []).forEach(function (item) {
        self.add(item, false);
      });
      self.more.hidden = self.items.length >= self.total;
      self.say(self.items.length ? '' : 'No media items found.');
    }).catch(function (error) {
      if (ticket === self.loading) {
        self.say(error.message, true);
      }
    });
  };

  /** Adds `item` to the list (first when `first`). */
  Picker.prototype.add = function (item, first) {
    var self = this;
    if (!item || !item.id) {
      return null;
    }
    var id = String(pixels(item.id));
    if (this.buttons[id]) {
      // Already listed (an upload, then the page that holds it).
      return this.buttons[id];
    }
    this.items.push(item);
    var entry = element('li');
    var button = element('button', {
      type: 'button',
      'class': 'mc-media-item',
      'aria-pressed': 'false',
      'data-id': id
    });
    this.buttons[id] = button;
    var thumbnail = item.sizes && (item.sizes.thumbnail || item.sizes.medium || item.sizes.full);
    if (item.type === 'image' && (thumbnail || item.url)) {
      button.appendChild(element('img', {
        src: thumbnail ? thumbnail.url : item.url,
        alt: '',
        loading: 'lazy'
      }));
    } else {
      button.appendChild(element('span', { 'class': 'mc-media-file' }, item.subtype || item.type));
    }
    button.appendChild(element('span', { 'class': 'mc-media-name' },
      item.title || item.filename || ''));
    button.addEventListener('click', function () {
      self.select(item, button);
    });
    entry.appendChild(button);
    if (first && this.list.firstChild) {
      this.list.insertBefore(entry, this.list.firstChild);
    } else {
      this.list.appendChild(entry);
    }
    return button;
  };

  Picker.prototype.select = function (item, button) {
    Array.prototype.forEach.call(this.list.querySelectorAll('.mc-media-item'), function (b) {
      b.setAttribute('aria-pressed', String(b === button));
    });
    this.selected = item;
    this.renderDetails();
  };

  /** A labelled select of `choices`. */
  function select(id, label, choices, value) {
    var wrap = element('label', { 'for': id }, label);
    var field = element('select', { id: id, 'class': 'ig-input' });
    choices.forEach(function (choice) {
      var option = element('option', { value: choice[0] }, choice[1]);
      option.selected = choice[0] === value;
      field.appendChild(option);
    });
    return { label: wrap, field: field };
  }

  /** The details of the selected item, and the Insert button. */
  Picker.prototype.renderDetails = function () {
    var details = this.details;
    var item = this.selected;
    details.textContent = '';
    if (!item) {
      details.appendChild(element('p', {}, 'Select a file, or upload one.'));
      return;
    }
    details.appendChild(element('h3', {}, 'Attachment details'));
    details.appendChild(element('p', { 'class': 'mc-media-filename' }, item.filename || ''));
    if (item.width && item.height) {
      details.appendChild(element('p', {}, pixels(item.width) + ' by ' + pixels(item.height) +
        ' pixels'));
    }
    this.altInput = null;
    this.captionInput = null;
    this.sizeSelect = null;
    this.alignSelect = null;
    if (item.type === 'image') {
      details.appendChild(element('label', { 'for': 'mc-media-alt' }, 'Alternative Text'));
      this.altInput = element('input', { type: 'text', id: 'mc-media-alt', 'class': 'ig-input' });
      this.altInput.value = item.alt || '';
      details.appendChild(this.altInput);
      details.appendChild(element('label', { 'for': 'mc-media-caption' }, 'Caption'));
      this.captionInput = element('textarea', { id: 'mc-media-caption', 'class': 'ig-input', rows: '2' });
      this.captionInput.value = item.caption || '';
      details.appendChild(this.captionInput);
      var sizes = SIZES.filter(function (size) {
        return item.sizes && item.sizes[size[0]];
      }).map(function (size) {
        var s = item.sizes[size[0]];
        return [size[0], size[1] + ' – ' + pixels(s.width) + ' × ' + pixels(s.height)];
      });
      if (!sizes.length) {
        sizes = [['full', 'Full Size']];
      }
      var preferred = item.sizes && item.sizes.medium ? 'medium' : 'full';
      var size = select('mc-media-size', 'Size', sizes, preferred);
      var align = select('mc-media-align', 'Alignment', ALIGNMENTS, 'none');
      this.sizeSelect = size.field;
      this.alignSelect = align.field;
      details.appendChild(align.label);
      details.appendChild(align.field);
      details.appendChild(size.label);
      details.appendChild(size.field);
    }
    details.appendChild(element('button', { type: 'submit', 'class': 'ig-btn ig-btn-primary' },
      'Insert into post'));
  };

  Picker.prototype.insert = function () {
    var item = this.selected;
    if (!item) {
      return;
    }
    var html = markup(
      item,
      this.sizeSelect ? this.sizeSelect.value : 'full',
      this.altInput ? this.altInput.value : '',
      this.alignSelect ? this.alignSelect.value : 'none',
      this.captionInput ? this.captionInput.value : ''
    );
    if (typeof api.insert === 'function') {
      api.insert(this.editorId, html);
    }
    this.dialog.close();
  };

  /** Uploads the chosen file, then selects it in the library. */
  Picker.prototype.upload = function () {
    var self = this;
    var file = this.fileInput.files && this.fileInput.files[0];
    if (!file) {
      this.say('Select a file to upload.', true);
      return;
    }
    var body = new FormData();
    body.append('async-upload', file, file.name);
    var post = editedPost();
    if (post) {
      body.append('post_id', post);
    }
    this.say('Uploading ' + file.name + '…');
    window.fetch(ADMIN + '/async-upload.php', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'X-CSRF-TOKEN': csrfToken(), Accept: 'application/json' },
      body: body
    }).then(readJson).then(function (answer) {
      self.fileInput.value = '';
      self.show('library');
      var button = self.add(answer.data, true);
      self.total += 1;
      if (button) {
        self.select(answer.data, button);
        button.focus();
      }
      self.say(file.name + ' was uploaded.');
    }).catch(function (error) {
      self.say(file.name + ' has failed to upload. ' + error.message, true);
    });
  };

  api.media = {
    open: function (editorId) {
      new Picker(editorId).open();
    },
    markup: markup
  };
}());
