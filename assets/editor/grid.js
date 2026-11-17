/*!
 * Mission Control's media library grid (Task A8). GPL-2.0-or-later.
 *
 * WordPress's Media Library in its grid mode: the attachments as thumbnails, newest first, a
 * search, "Load more", and the attachment details panel of the selected item, with its edit
 * link and Delete permanently. The page (`/mc-admin/attachment`) is Mission Control's; the
 * items come from `admin-ajax.php?action=query-attachments`, as the Add Media picker reads
 * them. Every value is written as text, never as HTML.
 */
(function () {
  'use strict';

  var root = document.getElementById('mc-media-grid');
  if (!root || !window.fetch) {
    return;
  }
  var ajax = root.getAttribute('data-ajax');
  var admin = root.getAttribute('data-admin');
  var token = root.getAttribute('data-token');
  var list = root.querySelector('.mc-grid-items');
  var more = root.querySelector('.mc-grid-more');
  var status = root.querySelector('.mc-grid-status');
  var search = root.querySelector('#mc-grid-search');
  var details = root.querySelector('.mc-grid-details');
  var PER_PAGE = 40;
  var page = 0;
  var totalPages = 1;
  var query = '';
  var items = {};
  /** The latest request: an older answer that arrives later is dropped. */
  var latest = 0;

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (text !== undefined && text !== null) {
      node.textContent = String(text);
    }
    return node;
  }

  /** The URL of the thumbnail an item shows: its `thumbnail` size, or the file for an image. */
  function thumbnail(item) {
    var sizes = item.sizes || {};
    var size = sizes.thumbnail || sizes.medium || sizes.full;
    return size && size.url ? size.url : null;
  }

  function load(reset) {
    if (reset) {
      page = 0;
      totalPages = 1;
      items = {};
      list.textContent = '';
    }
    if (page >= totalPages) {
      return;
    }
    page += 1;
    var request = ++latest;
    var url = new URL(ajax, document.baseURI);
    url.searchParams.set('action', 'query-attachments');
    url.searchParams.set('page', String(page));
    url.searchParams.set('per_page', String(PER_PAGE));
    if (query) {
      url.searchParams.set('search', query);
    }
    status.textContent = 'Loading…';
    more.hidden = true;
    fetch(url.href, { credentials: 'same-origin', headers: { Accept: 'application/json' } })
      .then(function (response) {
        if (request === latest) {
          totalPages = parseInt(response.headers.get('X-MC-TotalPages') || '1', 10) || 1;
        }
        return response.json();
      })
      .then(function (body) {
        if (request !== latest) {
          return;
        }
        if (!body || !body.success) {
          failed();
          return;
        }
        body.data.forEach(add);
        status.textContent = list.children.length ? '' : 'No media items found.';
        more.hidden = page >= totalPages;
      })
      .catch(function () {
        if (request === latest) {
          failed();
        }
      });
  }

  /** A page that could not be read: the message, and Load more to try it again. */
  function failed() {
    page -= 1;
    status.textContent = 'The media library could not be loaded.';
    more.hidden = false;
  }

  function add(item) {
    items[item.id] = item;
    var cell = el('li', 'mc-grid-item');
    var button = el('button', 'mc-grid-thumb');
    button.type = 'button';
    button.setAttribute('aria-label', item.title || item.filename || ('#' + item.id));
    var src = thumbnail(item);
    if (src) {
      var img = el('img');
      img.src = src;
      img.alt = '';
      img.loading = 'lazy';
      button.appendChild(img);
    } else {
      button.appendChild(el('span', 'mc-grid-file', item.filename));
    }
    button.addEventListener('click', function () {
      show(item.id, button);
    });
    cell.appendChild(button);
    list.appendChild(cell);
  }

  /** WordPress's attachment details: the item's facts, its links, and Delete permanently. */
  function show(id, from) {
    var item = items[id];
    if (!item) {
      return;
    }
    Array.prototype.forEach.call(list.querySelectorAll('.mc-grid-thumb'), function (b) {
      b.setAttribute('aria-pressed', b === from ? 'true' : 'false');
    });
    details.textContent = '';
    details.hidden = false;
    var heading = el('h2', null, 'Attachment details');
    heading.tabIndex = -1;
    details.appendChild(heading);
    var close = el('button', 'mc-grid-close', 'Close');
    close.type = 'button';
    close.addEventListener('click', function () {
      hide(from);
    });
    details.appendChild(close);
    var src = thumbnail(item);
    if (src && item.type === 'image') {
      var img = el('img', 'mc-grid-preview');
      img.src = (item.sizes && item.sizes.medium && item.sizes.medium.url) || src;
      img.alt = item.alt || '';
      details.appendChild(img);
    }
    var facts = el('dl', 'mc-grid-facts');
    var fact = function (label, value) {
      if (value === undefined || value === null || value === '') {
        return;
      }
      facts.appendChild(el('dt', null, label));
      facts.appendChild(el('dd', null, value));
    };
    fact('Uploaded on', item.dateFormatted);
    fact('Uploaded by', item.authorName);
    fact('File name', item.filename);
    fact('File type', item.mime);
    var full = item.sizes && item.sizes.full;
    if (full && full.width && full.height) {
      fact('Dimensions', full.width + ' by ' + full.height + ' pixels');
    }
    fact('Title', item.title);
    fact('Alternative Text', item.alt);
    fact('Caption', item.caption);
    fact('Description', item.description);
    details.appendChild(facts);
    var url = el('input', 'mc-grid-url');
    url.type = 'text';
    url.readOnly = true;
    url.id = 'mc-grid-url';
    url.value = item.url || '';
    var label = el('label', null, 'File URL:');
    label.htmlFor = 'mc-grid-url';
    details.appendChild(label);
    details.appendChild(url);
    var links = el('p', 'mc-grid-links');
    var edit = el('a', null, 'Edit more details');
    edit.href = item.editLink || (admin + '/attachment/' + item.id + '/edit');
    links.appendChild(edit);
    if (item.url) {
      links.appendChild(document.createTextNode(' | '));
      var view = el('a', null, 'View media file');
      view.href = item.url;
      links.appendChild(view);
    }
    details.appendChild(links);
    var form = el('form', 'mc-grid-delete');
    form.method = 'post';
    form.action = admin + '/attachment/' + item.id + '/delete';
    var hidden = el('input');
    hidden.type = 'hidden';
    hidden.name = '_token';
    hidden.value = token;
    form.appendChild(hidden);
    var remove = el('button', 'mc-grid-remove', 'Delete permanently');
    remove.type = 'submit';
    form.appendChild(remove);
    form.addEventListener('submit', function (event) {
      // WordPress's confirmation for a permanent delete.
      if (!window.confirm('You are about to permanently delete this item from your site.\nThis action cannot be undone.\n\'Cancel\' to stop, \'OK\' to delete.')) {
        event.preventDefault();
      }
    });
    details.appendChild(form);
    heading.focus();
  }

  /** Closes the details and gives the focus back to the item. */
  function hide(from) {
    details.hidden = true;
    if (from) {
      from.setAttribute('aria-pressed', 'false');
      from.focus();
    }
  }

  document.addEventListener('keydown', function (event) {
    if (event.key === 'Escape' && !details.hidden) {
      hide(list.querySelector('.mc-grid-thumb[aria-pressed="true"]'));
    }
  });

  more.addEventListener('click', function () {
    load(false);
  });
  var timer = null;
  search.addEventListener('input', function () {
    window.clearTimeout(timer);
    timer = window.setTimeout(function () {
      query = search.value.trim();
      details.hidden = true;
      load(true);
    }, 300);
  });
  load(true);
}());
