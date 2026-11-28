/**
 * Mission Control's menu builder: WordPress's Menus screen (`wp-admin/nav-menus.php` and
 * `nav-menu.js`) on top of Ignite's form of the Menus screen. The page carries the
 * "Add menu items" column (`#mc-menu-builder`, hidden) and the form's `items` field, the
 * builder's hidden input of JSON rows (`id`, `title`, `type`, `object_id`, `url`, `parent`,
 * `classes`, `new_tab`, `attr_title`, `xfn`, `description`), which the server reads.
 *
 * - The column shows beside the form: one box per post type (its published items, with a
 *   search and "Load more"), Custom Links (a URL and its text), one box per taxonomy. Each box
 *   has checkboxes, "Select All" and "Add to Menu", which adds the checked items at the end of
 *   the menu (`wpNavMenu.addItemToMenu`).
 * - The items field shows as the Menu Structure: the items as a nested list, in tree order (as
 *   `Walker_Nav_Menu_Edit` lists them). Each item opens ("Edit") on its settings, as
 *   `Walker_Nav_Menu_Edit::start_el` shows them: the URL (custom links), Navigation Label, Title
 *   Attribute, Open link in a new tab, CSS Classes, Link Relationship (XFN) and Description;
 *   the Move links; "Original:" for a post or term; Remove and Cancel.
 * - WordPress orders and nests the items by dragging them. Here the Move links do it, as
 *   nav-menu.js's keyboard links: Up one, Down one, Under (the previous item: make it a sub
 *   item), Out from under (the parent: the item and its sub items come after the parent's) and
 *   To the top. An item moves with its sub items.
 *
 * Every change is written back into the hidden field, which the form saves as before; nothing is
 * written until the user changes something, so the panel's unsaved changes alert stays quiet on
 * an unchanged form. A row's parent is the parent's `id` and its row number (`parent_row`), so
 * an item can be under a new item. Without this script the column stays hidden, and a save
 * keeps the items as they are.
 */
(function () {
  'use strict';

  var builder = document.getElementById('mc-menu-builder');
  var field = document.querySelector('[data-ignite-field="items"]');
  // The builder's hidden input (a textarea when the page was not given the builder's field).
  var textarea = field && field.querySelector('input[name="items"], textarea[name="items"]');
  var form = textarea && textarea.form;
  if (!builder || !textarea || !form || builder.getAttribute('data-started')) {
    return;
  }

  var rows;
  try {
    rows = textarea.value.trim() ? JSON.parse(textarea.value) : [];
  } catch (e) {
    // Text that is not JSON is left as it is: the save refuses it with its message.
    return;
  }
  if (!Array.isArray(rows)) {
    return;
  }
  builder.setAttribute('data-started', 'true');

  var searchUrl = builder.getAttribute('data-search-url');
  var maxDepth = parseInt(builder.getAttribute('data-max-depth'), 10) || 11;
  var CUSTOM = 'custom';

  /** The boxes by object: their kind (`post_type`, `taxonomy`) and singular label. */
  var sources = {};
  builder.querySelectorAll('.mc-menu-box').forEach(function (box) {
    sources[box.getAttribute('data-object')] = {
      kind: box.getAttribute('data-kind'),
      label: box.getAttribute('data-label')
    };
  });

  var nextKey = 1;

  /** The message of a custom link whose URL is not a link, as the server refuses it. */
  var INVALID_URL = 'Please enter a valid URL.';

  /** The protocols `esc_url` keeps (`wp_allowed_protocols`). */
  var PROTOCOLS = ['http', 'https', 'ftp', 'ftps', 'mailto', 'news', 'irc', 'irc6', 'ircs',
    'gopher', 'nntp', 'feed', 'telnet', 'mms', 'rtsp', 'sms', 'svn', 'tel', 'fax', 'xmpp',
    'webcal', 'urn'];

  /** WordPress's check of a custom link's URL (`nav-menu.js`, `addCustomLink`). */
  var URL_PATTERN = /^((\w+:)?\/\/\w.*|\w+:(?!\/\/$)|\/|\?|#)/;

  /**
   * Whether `address` is a link a custom item can hold, as the server checks it: no spaces,
   * WordPress's pattern (a protocol, or a path, query or fragment; a bare word such as
   * `notaurl` is refused), and an allowed protocol with something after it.
   */
  function validUrl(address) {
    var url = text(address).trim();
    if (url === '' || /[\s\u0000-\u001f\u007f]/.test(url) || !URL_PATTERN.test(url)) {
      return false;
    }
    if (/^[\/#?]/.test(url)) {
      return true;
    }
    if (!/^[A-Za-z0-9\-~+_.?#=!&;,\/:%@$|*'()\[\]\u0080-\uffff]+$/.test(url)) {
      return false;
    }
    var scheme = /^([A-Za-z][A-Za-z0-9+.\-]*):/.exec(url);
    if (!scheme) {
      url = 'http://' + url;
    } else if (PROTOCOLS.indexOf(scheme[1].toLowerCase()) === -1) {
      return false;
    }
    var at = url.indexOf('://');
    if (at === -1) {
      return url.slice(url.indexOf(':') + 1) !== '';
    }
    var host = url.slice(at + 3).split(/[\/?#]/)[0];
    return host !== '' && !/^[.\-]/.test(host);
  }

  /** Shows or clears the URL message under a custom item's URL `input`. */
  function urlMessage(input, valid) {
    var wrap = input.parentNode;
    var message = wrap.querySelector('.mc-menu-url-error');
    if (valid) {
      input.removeAttribute('aria-invalid');
      input.removeAttribute('aria-describedby');
      if (message) {
        wrap.removeChild(message);
      }
      return;
    }
    if (!message) {
      message = element('span', 'mc-menu-url-error ig-error', INVALID_URL);
      message.id = input.id + '-error';
      message.setAttribute('role', 'alert');
      wrap.appendChild(message);
    }
    input.setAttribute('aria-invalid', 'true');
    input.setAttribute('aria-describedby', message.id);
  }

  function text(value) {
    if (value === null || value === undefined) {
      return '';
    }
    return String(value);
  }

  function number(value) {
    var n = parseInt(value, 10);
    return isNaN(n) ? 0 : n;
  }

  // -------------------------------------------------------------------------------------------
  // The items

  /** An item of the structure, from a row of the field. */
  function itemOf(row) {
    var type = text(row.type).trim();
    if ((type === 'post_type' || type === 'taxonomy') && text(row.object).trim()) {
      type = text(row.object).trim();
    }
    return {
      key: nextKey++,
      id: number(row.id) || null,
      type: type,
      objectId: number(row.object_id) || null,
      url: text(row.url),
      title: text(row.title),
      attrTitle: text(row.attr_title),
      classes: text(row.classes),
      xfn: text(row.xfn),
      description: text(row.description),
      newTab: row.new_tab === true,
      depth: 0,
      original: '',
      invalid: false,
      pending: false
    };
  }

  /**
   * The items of `rows`, in tree order, each with its depth. A parent is found as the save
   * finds it: `parent_row`, else the row with that `id`, else a row number.
   */
  function read(rows) {
    var items = rows.map(itemOf);
    var parents = rows.map(function (row, index) {
      var at = number(row.parent_row);
      if (at > 0) {
        return at <= rows.length && at - 1 !== index ? at - 1 : -1;
      }
      var parent = number(row.parent);
      if (parent <= 0) {
        return -1;
      }
      for (var i = 0; i < items.length; i++) {
        if (items[i].id === parent) {
          return i === index ? -1 : i;
        }
      }
      return parent <= rows.length && parent - 1 !== index ? parent - 1 : -1;
    });
    // A loop puts its items at the top level, as the save does.
    parents = parents.map(function (parent, index) {
      var seen = {};
      seen[index] = true;
      var at = parent;
      while (at >= 0) {
        if (seen[at]) {
          return -1;
        }
        seen[at] = true;
        at = parents[at];
      }
      return parent;
    });
    var children = items.map(function () { return []; });
    var roots = [];
    parents.forEach(function (parent, index) {
      (parent >= 0 ? children[parent] : roots).push(index);
    });
    var ordered = [];
    function walk(index, depth) {
      var item = items[index];
      item.depth = Math.min(depth, maxDepth);
      ordered.push(item);
      children[index].forEach(function (child) {
        walk(child, depth + 1);
      });
    }
    roots.forEach(function (root) {
      walk(root, 0);
    });
    return ordered;
  }

  var items = read(rows);

  /** The rows of the items, for the field. */
  function rowsOf() {
    return items.map(function (item, index) {
      var parent = -1;
      for (var i = index - 1; i >= 0; i--) {
        if (items[i].depth === item.depth - 1) {
          parent = i;
          break;
        }
      }
      // `wp_update_nav_menu_item`: a label equal to the original title is stored empty.
      var title = item.title;
      if (item.type !== CUSTOM && item.original && title === item.original) {
        title = '';
      }
      var row = {
        id: item.id || '',
        title: title,
        type: item.type,
        object_id: item.type === CUSTOM ? '' : (item.objectId || ''),
        url: item.type === CUSTOM ? item.url : '',
        parent: parent >= 0 ? (items[parent].id || 0) : 0,
        classes: item.classes,
        new_tab: item.newTab,
        attr_title: item.attrTitle,
        xfn: item.xfn,
        description: item.description
      };
      if (parent >= 0) {
        row.parent_row = parent + 1;
      }
      return row;
    });
  }

  /** Writes the items into the field, and tells the form (and the panel's alert). */
  function save() {
    textarea.value = JSON.stringify(rowsOf());
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    textarea.dispatchEvent(new Event('change', { bubbles: true }));
  }

  /** The end (exclusive) of the block of item `index`: the item and its sub items. */
  function blockEnd(index) {
    var end = index + 1;
    while (end < items.length && items[end].depth > items[index].depth) {
      end++;
    }
    return end;
  }

  function previousSibling(index) {
    var depth = items[index].depth;
    for (var i = index - 1; i >= 0; i--) {
      if (items[i].depth === depth) {
        return i;
      }
      if (items[i].depth < depth) {
        return -1;
      }
    }
    return -1;
  }

  function nextSibling(index) {
    var end = blockEnd(index);
    return end < items.length && items[end].depth === items[index].depth ? end : -1;
  }

  function parentOf(index) {
    for (var i = index - 1; i >= 0; i--) {
      if (items[i].depth === items[index].depth - 1) {
        return i;
      }
    }
    return -1;
  }

  /** The deepest sub item of the block of `index`, relative to it. */
  function blockDepth(index) {
    var deepest = 0;
    for (var i = index; i < blockEnd(index); i++) {
      deepest = Math.max(deepest, items[i].depth - items[index].depth);
    }
    return deepest;
  }

  /** Takes the block of `index` out, its depths shifted by `shift`. */
  function takeBlock(index, shift) {
    var block = items.splice(index, blockEnd(index) - index);
    block.forEach(function (item) {
      item.depth += shift;
    });
    return block;
  }

  function insertBlock(at, block) {
    Array.prototype.splice.apply(items, [at, 0].concat(block));
  }

  var moves = {
    up: function (index) {
      var previous = previousSibling(index);
      if (previous < 0) {
        return;
      }
      insertBlock(previous, takeBlock(index, 0));
    },
    down: function (index) {
      var next = nextSibling(index);
      if (next < 0) {
        return;
      }
      var block = takeBlock(next, 0);
      insertBlock(index, block);
    },
    top: function (index) {
      if (index === 0) {
        return;
      }
      insertBlock(0, takeBlock(index, -items[index].depth));
    },
    under: function (index) {
      if (previousSibling(index) < 0 || items[index].depth + blockDepth(index) + 1 > maxDepth) {
        return;
      }
      for (var i = index; i < blockEnd(index); i++) {
        items[i].depth += 1;
      }
    },
    out: function (index) {
      var parent = parentOf(index);
      if (parent < 0) {
        return;
      }
      var end = blockEnd(parent);
      var block = takeBlock(index, -1);
      insertBlock(end - block.length, block);
    }
  };

  /** Removes item `index`; its sub items move up one level, as `removeMenuItem` does. */
  function remove(index) {
    var end = blockEnd(index);
    for (var i = index + 1; i < end; i++) {
      items[i].depth -= 1;
    }
    items.splice(index, 1);
  }

  // -------------------------------------------------------------------------------------------
  // The Menu Structure

  var structure = document.createElement('div');
  structure.className = 'mc-menu-structure';
  var instructions = document.createElement('p');
  instructions.className = 'mc-menu-instructions';
  var list = document.createElement('ul');
  list.className = 'mc-menu-items';
  list.setAttribute('aria-label', 'Menu Structure');
  var announce = document.createElement('p');
  announce.className = 'screen-reader-text mc-menu-announce';
  announce.setAttribute('role', 'status');
  announce.setAttribute('aria-live', 'polite');
  structure.appendChild(instructions);
  structure.appendChild(list);
  structure.appendChild(announce);

  /** The items whose settings are open, by key, with their values when opened (Cancel). */
  var open = {};

  function labelOf(item) {
    if (item.title) {
      return item.title;
    }
    if (item.original) {
      return item.original;
    }
    if (item.type === CUSTOM && item.url) {
      return item.url;
    }
    return '(no label)';
  }

  function typeLabel(item) {
    if (item.type === CUSTOM) {
      return 'Custom Link';
    }
    return sources[item.type] ? sources[item.type].label : item.type;
  }

  function element(tag, className, content) {
    var node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (content !== undefined) {
      node.textContent = content;
    }
    return node;
  }

  function button(content, className, onClick) {
    var node = element('button', className, content);
    node.type = 'button';
    node.addEventListener('click', onClick);
    return node;
  }

  /** A setting of item `item`: a labelled input that writes `key` of the item. */
  function setting(item, labelText, key, kind, onInput) {
    var wrap = element('p', 'mc-menu-setting mc-menu-setting-' + key);
    var id = 'mc-menu-item-' + key + '-' + item.key;
    var label = element('label', 'ig-label', labelText);
    label.htmlFor = id;
    var input;
    if (kind === 'textarea') {
      input = element('textarea', 'ig-input');
      input.rows = 3;
    } else {
      input = element('input', 'ig-input');
      input.type = kind || 'text';
    }
    input.id = id;
    input.value = item[key];
    input.addEventListener('input', function () {
      item[key] = input.value;
      save();
      if (onInput) {
        onInput();
      }
    });
    wrap.appendChild(label);
    wrap.appendChild(input);
    if (key === 'url') {
      // WordPress's item URL is a text field (`edit-menu-item-url`): the check is the
      // builder's, with its message under the field, not the browser's.
      input.setAttribute('inputmode', 'url');
      input.addEventListener('blur', function () {
        urlMessage(input, validUrl(input.value));
      });
      input.addEventListener('input', function () {
        if (input.getAttribute('aria-invalid') && validUrl(input.value)) {
          urlMessage(input, true);
        }
      });
      if (!validUrl(item.url)) {
        urlMessage(input, false);
      }
    }
    return wrap;
  }

  function render(focusKey, focusAction) {
    list.textContent = '';
    instructions.textContent = items.length
      ? 'Put each item in the order you prefer with its Move links: open an item with Edit ' +
        'to see them and its other settings.'
      : 'Add menu items from the column on the left.';
    items.forEach(function (item, index) {
      list.appendChild(row(item, index));
    });
    if (focusKey) {
      var target = list.querySelector('[data-key="' + focusKey + '"] ' +
        (focusAction ? '[data-move="' + focusAction + '"]' : '.mc-menu-item-edit'));
      if (!target) {
        target = list.querySelector('[data-key="' + focusKey + '"] .mc-menu-item-edit');
      }
      if (target) {
        target.focus();
      }
    }
  }

  function row(item, index) {
    var node = element('li', 'mc-menu-item mc-menu-item-depth-' + item.depth);
    node.setAttribute('data-key', item.key);
    node.style.marginLeft = (item.depth * 30) + 'px';

    var bar = element('div', 'mc-menu-item-bar');
    var title = element('span', 'mc-menu-item-title', labelOf(item));
    bar.appendChild(title);
    if (item.depth > 0) {
      bar.appendChild(element('span', 'mc-menu-item-sub', 'sub item'));
    }
    var type = typeLabel(item);
    if (item.invalid) {
      type += ' (Invalid)';
      node.classList.add('mc-menu-item-invalid');
    } else if (item.pending) {
      type += ' (Pending)';
    }
    bar.appendChild(element('span', 'mc-menu-item-type', type));
    var edit = button('Edit', 'mc-menu-item-edit', function () {
      if (open[item.key]) {
        delete open[item.key];
      } else {
        open[item.key] = snapshot(item);
      }
      render(item.key);
    });
    edit.setAttribute('aria-expanded', open[item.key] ? 'true' : 'false');
    edit.setAttribute('aria-label', 'Edit ' + labelOf(item) + ' (' + type + ')');
    bar.appendChild(edit);
    node.appendChild(bar);

    if (open[item.key]) {
      node.appendChild(settings(item, index, title));
    }
    return node;
  }

  function snapshot(item) {
    return {
      url: item.url,
      title: item.title,
      attrTitle: item.attrTitle,
      newTab: item.newTab,
      classes: item.classes,
      xfn: item.xfn,
      description: item.description
    };
  }

  function settings(item, index, title) {
    var panel = element('div', 'mc-menu-item-settings');
    var relabel = function () {
      title.textContent = labelOf(item);
    };
    if (item.type === CUSTOM) {
      panel.appendChild(setting(item, 'URL', 'url', 'text', relabel));
    }
    var navigationLabel = setting(item, 'Navigation Label', 'title', 'text', relabel);
    // An empty label shows the original title, as the site does.
    navigationLabel.querySelector('input').placeholder = item.original;
    panel.appendChild(navigationLabel);
    panel.appendChild(setting(item, 'Title Attribute', 'attrTitle', 'text'));

    var target = element('p', 'mc-menu-setting mc-menu-setting-target');
    var targetLabel = element('label');
    var box = element('input');
    box.type = 'checkbox';
    box.checked = item.newTab;
    box.addEventListener('change', function () {
      item.newTab = box.checked;
      save();
    });
    targetLabel.appendChild(box);
    targetLabel.appendChild(document.createTextNode(' Open link in a new tab'));
    target.appendChild(targetLabel);
    panel.appendChild(target);

    panel.appendChild(setting(item, 'CSS Classes (optional)', 'classes', 'text'));
    panel.appendChild(setting(item, 'Link Relationship (XFN)', 'xfn', 'text'));
    var description = setting(item, 'Description', 'description', 'textarea');
    description.appendChild(element('span', 'mc-menu-help',
      'The description will be displayed in the menu if the active theme supports it.'));
    panel.appendChild(description);

    panel.appendChild(moveLinks(item, index));

    if (item.type !== CUSTOM && item.original) {
      panel.appendChild(element('p', 'mc-menu-original', 'Original: ' + item.original));
    }

    var actions = element('p', 'mc-menu-item-actions');
    actions.appendChild(button('Remove', 'mc-menu-item-delete', function () {
      var at = items.indexOf(item);
      delete open[item.key];
      remove(at);
      save();
      var next = items[Math.min(at, items.length - 1)];
      render(next ? next.key : null);
      announce.textContent = 'Menu item removed: ' + labelOf(item);
    }));
    actions.appendChild(document.createTextNode(' | '));
    actions.appendChild(button('Cancel', 'mc-menu-item-cancel', function () {
      var before = open[item.key];
      var changed = Object.keys(before).some(function (key) {
        return before[key] !== item[key];
      });
      Object.keys(before).forEach(function (key) {
        item[key] = before[key];
      });
      delete open[item.key];
      if (changed) {
        save();
      }
      render(item.key);
    }));
    panel.appendChild(actions);
    return panel;
  }

  function moveLinks(item, index) {
    var wrap = element('p', 'mc-menu-move');
    wrap.appendChild(element('span', 'mc-menu-move-label', 'Move '));
    var links = [];
    var previous = previousSibling(index);
    var parent = parentOf(index);
    if (previous >= 0) {
      links.push(['up', 'Up one']);
    }
    if (nextSibling(index) >= 0) {
      links.push(['down', 'Down one']);
    }
    if (previous >= 0 && item.depth + blockDepth(index) + 1 <= maxDepth) {
      links.push(['under', 'Under ' + labelOf(items[previous])]);
    }
    if (parent >= 0) {
      links.push(['out', 'Out from under ' + labelOf(items[parent])]);
    }
    if (index > 0) {
      links.push(['top', 'To the top']);
    }
    links.forEach(function (link) {
      var move = button(link[1], 'mc-menu-move-' + link[0], function () {
        moves[link[0]](items.indexOf(item));
        save();
        render(item.key, link[0]);
        var at = items.indexOf(item);
        announce.textContent = labelOf(item) + ': menu item ' + (at + 1) + ' of ' +
          items.length + (item.depth > 0 ? ', sub item under ' +
          labelOf(items[parentOf(at)]) : '') + '.';
      });
      move.setAttribute('data-move', link[0]);
      wrap.appendChild(move);
    });
    return wrap;
  }

  // -------------------------------------------------------------------------------------------
  // The titles of the menu's posts and terms

  function query(kind, object, params) {
    var url = new URL(searchUrl, document.baseURI);
    url.searchParams.set('kind', kind);
    url.searchParams.set('object', object);
    Object.keys(params).forEach(function (key) {
      if (params[key] !== '' && params[key] !== null && params[key] !== undefined) {
        url.searchParams.set(key, params[key]);
      }
    });
    return fetch(url.href, { credentials: 'same-origin', headers: { Accept: 'application/json' } })
      .then(function (response) {
        if (!response.ok) {
          throw new Error(String(response.status));
        }
        return response.json();
      });
  }

  /** Reads the original titles of the items that link to posts and terms. */
  function readOriginals() {
    var byType = {};
    items.forEach(function (item) {
      if (item.type !== CUSTOM && item.objectId && sources[item.type]) {
        (byType[item.type] = byType[item.type] || []).push(item.objectId);
      }
    });
    var requests = Object.keys(byType).map(function (type) {
      return query(sources[type].kind, type, { include: byType[type].join(',') })
        .then(function (found) {
          var titles = {};
          found.items.forEach(function (entry) {
            titles[entry.id] = entry;
          });
          items.forEach(function (item) {
            if (item.type !== type || !item.objectId) {
              return;
            }
            var entry = titles[item.objectId];
            item.invalid = !entry || entry.status === 'trash' || entry.status === 'auto-draft';
            item.pending = !!entry && entry.status === 'pending';
            item.original = entry ? entry.title : '';
          });
        })
        .catch(function () {});
    });
    Promise.all(requests).then(function () {
      render();
    });
  }

  // -------------------------------------------------------------------------------------------
  // The "Add menu items" column

  function addItems(added) {
    added.forEach(function (item) {
      items.push(item);
    });
    save();
    render();
    announce.textContent = added.length === 1
      ? 'Menu item added: ' + labelOf(added[0])
      : added.length + ' menu items added.';
  }

  function postBox(box) {
    var kind = box.getAttribute('data-kind');
    var object = box.getAttribute('data-object');
    var checklist = box.querySelector('.mc-menu-checklist');
    var status = box.querySelector('.mc-menu-status');
    var more = box.querySelector('.mc-menu-more');
    var search = box.querySelector('input[type="search"]');
    var selectAll = box.querySelector('.mc-menu-select-all');
    var add = box.querySelector('.mc-menu-add');
    var page = 0;
    var loaded = false;
    var timer = null;
    var serial = 0;

    function load(reset) {
      if (reset) {
        page = 0;
        checklist.textContent = '';
        selectAll.checked = false;
      }
      var ask = ++serial;
      status.textContent = 'Loading…';
      more.hidden = true;
      query(kind, object, { q: search.value.trim(), page: page + 1 })
        .then(function (found) {
          if (ask !== serial) {
            return;
          }
          page += 1;
          found.items.forEach(function (entry) {
            var li = element('li');
            var label = element('label');
            var box = element('input');
            box.type = 'checkbox';
            box.value = entry.id;
            box.setAttribute('data-title', entry.title);
            label.appendChild(box);
            label.appendChild(document.createTextNode(' ' + entry.title));
            li.appendChild(label);
            checklist.appendChild(li);
          });
          status.textContent = checklist.children.length ? '' : 'No results found.';
          more.hidden = !found.more;
        })
        .catch(function () {
          if (ask === serial) {
            status.textContent = 'The items could not be loaded.';
          }
        });
    }

    box.addEventListener('toggle', function () {
      if (box.open && !loaded) {
        loaded = true;
        load(true);
      }
    });
    search.addEventListener('input', function () {
      clearTimeout(timer);
      timer = setTimeout(function () {
        load(true);
      }, 300);
    });
    search.addEventListener('keydown', function (event) {
      if (event.key === 'Enter') {
        event.preventDefault();
        clearTimeout(timer);
        load(true);
      }
    });
    more.addEventListener('click', function () {
      load(false);
    });
    selectAll.addEventListener('change', function () {
      checklist.querySelectorAll('input[type="checkbox"]').forEach(function (check) {
        check.checked = selectAll.checked;
      });
    });
    add.addEventListener('click', function () {
      var checked = checklist.querySelectorAll('input[type="checkbox"]:checked');
      if (!checked.length) {
        status.textContent = 'Select the items to add.';
        return;
      }
      var added = Array.prototype.map.call(checked, function (check) {
        check.checked = false;
        var item = itemOf({ type: object, object_id: check.value, title: '' });
        item.original = check.getAttribute('data-title');
        return item;
      });
      selectAll.checked = false;
      status.textContent = '';
      addItems(added);
    });
  }

  function customBox(box) {
    var url = box.querySelector('.mc-custom-url');
    var label = box.querySelector('.mc-custom-text');
    var status = box.querySelector('.mc-menu-status');
    var add = box.querySelector('.mc-menu-add-custom');

    function addLink() {
      var address = url.value.trim();
      // nav-menu.js's `addCustomLink`: a URL is needed, and it must be a link.
      if (address === '' || address === 'https://' || address === 'http://') {
        url.setAttribute('aria-invalid', 'true');
        status.textContent = 'Enter the URL of the link.';
        url.focus();
        return;
      }
      if (!validUrl(address)) {
        url.setAttribute('aria-invalid', 'true');
        status.textContent = INVALID_URL;
        url.focus();
        return;
      }
      url.removeAttribute('aria-invalid');
      status.textContent = '';
      addItems([itemOf({ type: CUSTOM, url: address, title: label.value.trim() })]);
      url.value = 'https://';
      label.value = '';
    }

    add.addEventListener('click', addLink);
    [url, label].forEach(function (input) {
      input.addEventListener('keydown', function (event) {
        if (event.key === 'Enter') {
          event.preventDefault();
          addLink();
        }
      });
    });
  }

  builder.querySelectorAll('.mc-menu-box').forEach(function (box) {
    if (box.getAttribute('data-kind') === CUSTOM) {
      customBox(box);
    } else {
      postBox(box);
    }
  });
  // The first box opens, as WordPress opens Pages.
  var first = builder.querySelector('.mc-menu-box');
  if (first) {
    first.open = true;
    first.dispatchEvent(new Event('toggle'));
  }

  // -------------------------------------------------------------------------------------------
  // The page: the column beside the form, the structure in place of the textarea

  var columns = element('div', 'mc-menu-columns');
  var main = element('div', 'mc-menu-main');
  form.parentNode.insertBefore(columns, form);
  columns.appendChild(builder);
  columns.appendChild(main);
  main.appendChild(form);
  // A custom link whose URL is not a link stops the save: its settings open with the message
  // under its URL, which takes the focus, as the server would refuse the save.
  form.addEventListener('submit', function (event) {
    var bad = items.filter(function (item) {
      return item.type === CUSTOM && !validUrl(item.url);
    });
    if (!bad.length) {
      return;
    }
    event.preventDefault();
    bad.forEach(function (item) {
      if (!open[item.key]) {
        open[item.key] = snapshot(item);
      }
    });
    render();
    var input = list.querySelector('[data-key="' + bad[0].key + '"] .mc-menu-setting-url input');
    if (input) {
      urlMessage(input, false);
      input.focus();
    }
    announce.textContent = INVALID_URL;
  });
  var deleteForm = document.querySelector('form.mc-menu-delete');
  if (deleteForm) {
    main.appendChild(deleteForm);
    deleteForm.addEventListener('submit', function (event) {
      if (!window.confirm(deleteForm.getAttribute('data-confirm'))) {
        event.preventDefault();
      }
    });
  }

  // The field (its label and hint, when it shows them) gives way to the structure; an error
  // stays.
  var label = field.querySelector('label[for="items"]');
  var hint = textarea.nextElementSibling;
  textarea.hidden = true;
  if (label) {
    label.hidden = true;
  }
  if (hint && hint.tagName === 'P' && !/error/.test(hint.className)) {
    hint.hidden = true;
  }
  textarea.parentNode.insertBefore(structure, textarea);
  builder.hidden = false;
  render();
  readOriginals();
}());
