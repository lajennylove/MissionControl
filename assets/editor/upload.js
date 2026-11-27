/*!
 * Mission Control's upload page (`/mc-admin/attachment/new`). GPL-2.0-or-later.
 *
 * WordPress's `media-new.php` with its multi-file uploader (`media_upload_form` and plupload's
 * `handlers.js`): files dropped on "Drop files to upload", or chosen with "Select Files", are
 * uploaded one by one to `/mc-admin/async-upload.php` with the page's CSRF token, and each is
 * listed below with its thumbnail and an Edit link, or with WordPress's "“name” has failed to
 * upload." and the reason. Without this script the page is the browser uploader: the file
 * field and Upload, which post the form.
 *
 * Every value is written as text, never as HTML.
 */
(function () {
  'use strict';

  var root = document.getElementById('mc-upload');
  if (!root || !window.fetch || !window.FormData) {
    return;
  }
  var ajax = root.getAttribute('data-ajax');
  var token = root.getAttribute('data-token');
  var max = parseInt(root.getAttribute('data-max') || '0', 10) || 0;
  var input = root.querySelector('#async-upload');
  var area = root.querySelector('#drag-drop-area');
  var items = root.querySelector('#media-items');
  var browser = root.querySelector('.mc-browser-upload');
  var label = root.querySelector('.mc-select-files');
  if (!input || !area || !items) {
    return;
  }

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

  // The multi-file uploader: "Select Files" is a button that opens the file field, which stays
  // in the page (out of sight) and takes several files; the browser uploader's Upload goes.
  input.multiple = true;
  input.required = false;
  input.classList.add('mc-visually-hidden');
  input.tabIndex = -1;
  if (browser) {
    var submit = browser.querySelector('button[type="submit"]');
    if (submit) {
      submit.hidden = true;
    }
  }
  var select = el('button', 'ig-btn ig-btn-outline mc-select-files', 'Select Files');
  select.type = 'button';
  select.id = 'plupload-browse-button';
  select.setAttribute('aria-describedby', 'mc-upload-limit');
  select.addEventListener('click', function () {
    input.click();
  });
  if (label && label.parentNode) {
    label.parentNode.replaceChild(select, label);
  }

  input.addEventListener('change', function () {
    var files = Array.prototype.slice.call(input.files || []);
    files.forEach(upload);
    input.value = '';
  });

  // Dropping files on the area, as plupload's drop element.
  ['dragenter', 'dragover'].forEach(function (name) {
    area.addEventListener(name, function (event) {
      event.preventDefault();
      area.classList.add('mc-drag-over');
    });
  });
  ['dragleave', 'dragend'].forEach(function (name) {
    area.addEventListener(name, function () {
      area.classList.remove('mc-drag-over');
    });
  });
  area.addEventListener('drop', function (event) {
    event.preventDefault();
    area.classList.remove('mc-drag-over');
    var files = event.dataTransfer ? Array.prototype.slice.call(event.dataTransfer.files) : [];
    files.forEach(upload);
  });

  /** WordPress's `size_format` with no decimals, for the size limit's message. */
  function sizeFormat(bytes) {
    var units = [['TB', 1099511627776], ['GB', 1073741824], ['MB', 1048576], ['KB', 1024]];
    for (var i = 0; i < units.length; i++) {
      if (bytes >= units[i][1]) {
        return Math.round(bytes / units[i][1]) + ' ' + units[i][0];
      }
    }
    return bytes + ' B';
  }

  /** The row of one upload, in the list under the area. */
  function row(file) {
    var item = el('li', 'mc-upload-item');
    item.appendChild(el('span', 'mc-upload-name', file.name));
    var state = el('span', 'mc-upload-state', 'Uploading…');
    item.appendChild(state);
    items.insertBefore(item, items.firstChild);
    return { item: item, state: state };
  }

  /** WordPress's error box: "“name” has failed to upload." and the reason, with Dismiss. */
  function failed(entry, name, message) {
    var item = entry.item;
    item.textContent = '';
    item.className = 'mc-upload-item mc-upload-failed';
    item.setAttribute('role', 'alert');
    var text = el('p');
    text.appendChild(el('strong', null, '“' + name + '” has failed to upload.'));
    if (message) {
      text.appendChild(document.createElement('br'));
      text.appendChild(document.createTextNode(message));
    }
    var dismiss = el('button', 'mc-upload-dismiss', 'Dismiss');
    dismiss.type = 'button';
    dismiss.addEventListener('click', function () {
      item.remove();
      select.focus();
    });
    item.appendChild(dismiss);
    item.appendChild(text);
  }

  /** The uploaded item: its thumbnail (with its alternative text), its title and Edit. */
  function uploaded(entry, attachment) {
    var item = entry.item;
    item.textContent = '';
    item.className = 'mc-upload-item mc-upload-done';
    var sizes = attachment.sizes || {};
    var thumb = sizes.thumbnail || sizes.medium || sizes.full;
    if (attachment.type === 'image' && thumb && thumb.url) {
      var img = el('img', 'mc-upload-thumb');
      img.src = thumb.url;
      img.alt = attachment.alt || '';
      img.width = 60;
      img.height = 60;
      item.appendChild(img);
    }
    item.appendChild(el('span', 'mc-upload-name', attachment.title || attachment.filename || ''));
    var edit = el('a', 'mc-upload-edit', 'Edit');
    edit.href = attachment.editLink || '#';
    item.appendChild(edit);
  }

  function upload(file) {
    var entry = row(file);
    if (max && file.size > max) {
      // plupload's check before sending (`file_exceeds_size_limit`).
      failed(entry, file.name, file.name + ' exceeds the maximum upload size for this site (' +
        sizeFormat(max) + ').');
      return;
    }
    var body = new FormData();
    body.append('async-upload', file, file.name);
    fetch(ajax, {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'X-CSRF-TOKEN': token, Accept: 'application/json' },
      body: body
    }).then(function (response) {
      var type = response.headers.get('Content-Type') || '';
      if (type.indexOf('application/json') === -1) {
        throw new Error('Your session has expired. Please sign in again.');
      }
      return response.json();
    }).then(function (answer) {
      if (!answer || answer.success !== true) {
        var message = answer && answer.data && answer.data.message;
        throw new Error(message || 'The file could not be stored. Try again in a moment.');
      }
      uploaded(entry, answer.data);
    }).catch(function (error) {
      failed(entry, file.name, error.message);
    });
  }
}());
