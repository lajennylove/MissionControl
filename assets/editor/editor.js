/*!
 * Mission Control's classic editor (Task 6.3, design 5.4). GPL-2.0-or-later.
 *
 * Ignite's form loads this script (registered with `register_editor`) on a form with a rich
 * text field. It turns each `textarea[data-ignite-editor="wysiwyg"]` into WordPress's classic
 * editor: TinyMCE (bundled next to this file, under `tinymce/`) with WordPress's toolbar, the
 * Visual and Text tabs and the Add Media button. The textarea stays the form field: Visual
 * writes into it when the form is submitted, Text is the textarea itself.
 *
 * Every file comes from this site (TinyMCE's base URL is next to this script): no request goes
 * elsewhere. The `ver` query of this script's own URL is passed on to every file TinyMCE loads,
 * so each can be cached for long.
 *
 * For other scripts (the media library, Task 6.4), `window.mcEditor` offers:
 *
 * - `mcEditor.media`: set it to `{ open: function (editorId) { ... } }`; Add Media then calls
 *   `open` with the textarea's id instead of the built-in "Insert from URL" dialog.
 * - `mcEditor.insert(editorId, html)`: inserts HTML at the cursor of that editor, in the Visual
 *   or the Text tab (WordPress's `send_to_editor`).
 * - `mcEditor.isVisual(editorId)`: whether that editor shows the Visual tab.
 */
(function () {
  'use strict';

  var script = document.currentScript ||
    document.querySelector('script[src*="/js/editor.js"]');
  if (!script || !script.src) {
    return;
  }
  var scriptUrl = new URL(script.src, document.baseURI);
  var ver = scriptUrl.searchParams.get('ver') || '';
  var query = ver ? '?ver=' + encodeURIComponent(ver) : '';
  var tinymceBase = new URL('tinymce', scriptUrl).href;
  var stylesheet = new URL('../css/editor.css', scriptUrl).href + query;

  /** The key under which the last tab used is remembered, as WordPress's `editor` setting. */
  var MODE_KEY = 'mc-editor-mode';

  /** The key under which the Toolbar Toggle is remembered, as WordPress's `hidetb` setting. */
  var ADV_KEY = 'mc-editor-adv';

  /** A transparent GIF, the source of the Read More placeholder. */
  var BLANK_GIF = 'data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7';

  /** The block-level tags `autop` does not wrap in paragraphs (WordPress's `wpautop`). */
  var BLOCKS = 'table|thead|tfoot|caption|col|colgroup|tbody|tr|td|th|div|dl|dd|dt|ul|ol|li' +
    '|pre|form|map|area|blockquote|address|math|style|p|h[1-6]|hr|fieldset|legend|section' +
    '|article|aside|hgroup|header|footer|nav|figure|figcaption|details|menu|summary';

  var api = window.mcEditor = window.mcEditor || {};
  if (api.loaded) {
    return;
  }
  api.loaded = true;
  api.media = api.media || null;

  function escapeAttribute(text) {
    return String(text)
      .replace(/&/g, '&amp;')
      .replace(/"/g, '&quot;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }

  function unescapeAttribute(text) {
    return String(text)
      .replace(/&quot;/g, '"')
      .replace(/&lt;/g, '<')
      .replace(/&gt;/g, '>')
      .replace(/&#96;/g, '`')
      .replace(/&amp;/g, '&');
  }

  /**
   * Paragraphs for text written without them, as WordPress's `wp.editor.autop` does before the
   * Visual tab shows it: a blank line starts a paragraph, a single line break becomes `<br />`.
   * HTML that already has its paragraphs comes out the same.
   */
  function autop(text) {
    var keepLineBreaks = false;
    var keepBr = false;
    if (text.indexOf('\n') === -1 && text.indexOf('\r') === -1) {
      return text;
    }
    text = text.replace(/\r\n|\r/g, '\n');
    if (text.indexOf('<object') !== -1) {
      text = text.replace(/<object[\s\S]+?<\/object>/g, function (a) {
        return a.replace(/\n+/g, '');
      });
    }
    text = text.replace(/<[^<>]+>/g, function (a) {
      return a.replace(/[\n\t ]+/g, ' ');
    });
    if (text.indexOf('<pre') !== -1 || text.indexOf('<script') !== -1) {
      keepLineBreaks = true;
      text = text.replace(/<(pre|script)[^>]*>[\s\S]*?<\/\1>/g, function (a) {
        return a.replace(/\n/g, '<mc-line-break>');
      });
    }
    if (text.indexOf('<figcaption') !== -1) {
      text = text.replace(/\s*(<figcaption[^>]*>)/g, '$1');
      text = text.replace(/<\/figcaption>\s*/g, '</figcaption>');
    }
    if (text.indexOf('[caption') !== -1) {
      keepBr = true;
      text = text.replace(/\[caption[\s\S]+?\[\/caption\]/g, function (a) {
        a = a.replace(/<br([^>]*)>/g, '<mc-temp-br$1>');
        a = a.replace(/<[^<>]+>/g, function (b) {
          return b.replace(/[\n\t ]+/, ' ');
        });
        return a.replace(/\s*\n\s*/g, '<mc-temp-br />');
      });
    }
    text = text + '\n\n';
    text = text.replace(/<br \/>\s*<br \/>/gi, '\n\n');
    text = text.replace(new RegExp('(<(?:' + BLOCKS + ')(?: [^>]*)?>)', 'gi'), '\n\n$1');
    text = text.replace(new RegExp('(</(?:' + BLOCKS + ')>)', 'gi'), '$1\n\n');
    text = text.replace(/<hr( [^>]*)?>/gi, '<hr$1>\n\n');
    text = text.replace(/\s*<option/gi, '<option');
    text = text.replace(/<\/option>\s*/gi, '</option>');
    text = text.replace(/\n\s*\n+/g, '\n\n');
    text = text.replace(/([\s\S]+?)\n\n/g, '<p>$1</p>\n');
    text = text.replace(/<p>\s*?<\/p>/gi, '');
    text = text.replace(new RegExp('<p>\\s*(</?(?:' + BLOCKS + ')(?: [^>]*)?>)\\s*</p>', 'gi'), '$1');
    text = text.replace(/<p>(<li.+?)<\/p>/gi, '$1');
    text = text.replace(/<p>\s*<blockquote([^>]*)>/gi, '<blockquote$1><p>');
    text = text.replace(/<\/blockquote>\s*<\/p>/gi, '</p></blockquote>');
    text = text.replace(new RegExp('<p>\\s*(</?(?:' + BLOCKS + ')(?: [^>]*)?>)', 'gi'), '$1');
    text = text.replace(new RegExp('(</?(?:' + BLOCKS + ')(?: [^>]*)?>)\\s*</p>', 'gi'), '$1');
    text = text.replace(/(<br[^>]*>)\s*\n/gi, '$1');
    text = text.replace(/\s*\n/g, '<br />\n');
    text = text.replace(new RegExp('(</?(?:' + BLOCKS + ')[^>]*>)\\s*<br />', 'gi'), '$1');
    text = text.replace(/<br \/>(\s*<\/?(?:p|li|div|dl|dd|dt|th|pre|td|ul|ol)>)/gi, '$1');
    text = text.replace(
      /(?:<p>|<br ?\/?>)*\s*\[caption([^\[]+)\[\/caption\]\s*(?:<\/p>|<br ?\/?>)*/gi,
      '[caption$1[/caption]'
    );
    text = text.replace(/(<(?:div|th|td|form|fieldset|dd)[^>]*>)(.*?)<\/p>/g, function (a, b, c) {
      if (c.match(/<p( [^>]*)?>/)) {
        return a;
      }
      return b + '<p>' + c + '</p>';
    });
    if (keepLineBreaks) {
      text = text.replace(/<mc-line-break>/g, '\n');
    }
    if (keepBr) {
      text = text.replace(/<mc-temp-br([^>]*)>/g, '<br$1>');
    }
    return text;
  }

  /** The Visual tab's stand-in for `<!--more-->` and `<!--nextpage-->` (WordPress's wpmore). */
  function placeholder(kind, text) {
    var title = kind === 'more' ? 'Read more…' : 'Page break';
    return '<img src="' + BLANK_GIF + '" class="mce-mc-' + kind + '" data-mc-tag="' + kind +
      '" data-mc-text="' + escapeAttribute(text || '') + '" data-mce-resize="false"' +
      ' data-mce-placeholder="1" alt="" title="' + title + '" />';
  }

  function toPlaceholders(html) {
    return html.replace(/<!--(more|nextpage)(.*?)-->/g, function (match, kind, text) {
      return placeholder(kind, text);
    });
  }

  function fromPlaceholders(html) {
    var tag = /<img\b[^>]*\bdata-mc-tag="(more|nextpage)"[^>]*>/g;
    html = html.replace(new RegExp('<p>\\s*(' + tag.source + ')\\s*</p>', 'g'), '$1');
    return html.replace(tag, function (img, kind) {
      var text = /\bdata-mc-text="([^"]*)"/.exec(img);
      return '<!--' + kind + (text ? unescapeAttribute(text[1]) : '') + '-->';
    });
  }

  /** WordPress's classic editor settings (`_WP_Editors::editor_settings`), for TinyMCE 7. */
  var settings = {
    license_key: 'gpl',
    base_url: tinymceBase,
    suffix: '.min',
    cache_suffix: ver ? 'ver=' + encodeURIComponent(ver) : '',
    plugins: 'charmap fullscreen help link lists',
    menubar: false,
    branding: false,
    promotion: false,
    toolbar_mode: 'wrap',
    // WordPress 4.7+'s two rows (Ruling R130); the second shows with the Toolbar Toggle.
    toolbar1: 'blocks bold italic bullist numlist blockquote alignleft aligncenter ' +
      'alignright link mcmore fullscreen mcadv',
    toolbar2: 'strikethrough hr forecolor pastetext removeformat charmap outdent indent ' +
      'undo redo help',
    block_formats: 'Paragraph=p;Heading 1=h1;Heading 2=h2;Heading 3=h3;Heading 4=h4;' +
      'Heading 5=h5;Heading 6=h6;Preformatted=pre',
    // URLs stay as written: WordPress sets these so `/mc-content/uploads/...` is not made
    // relative to the admin page.
    relative_urls: false,
    remove_script_host: false,
    convert_urls: false,
    entity_encoding: 'raw',
    browser_spellcheck: true,
    end_container_on_empty_block: true,
    keep_styles: false,
    // Iframes and embeds stay as written, as WordPress's editor keeps them (only users with
    // `unfiltered_html` reach the post screens, Ruling R115).
    sandbox_iframes: false,
    convert_unsafe_embeds: false,
    resize: true,
    height: 400,
    // TinyMCE never writes the textarea by itself (on submit, on unload, through the form's
    // `submit()`): this script does, and only from the Visual tab. In the Text tab, the textarea
    // is what the user typed.
    add_form_submit_trigger: false,
    add_unload_trigger: false,
    submit_patch: false,
    content_style:
      'img.mce-mc-more,img.mce-mc-nextpage{display:block;width:100%;height:16px;' +
      'margin:15px auto 0;border:0;border-top:2px dashed #8c8f94;cursor:default}' +
      'img.mce-mc-nextpage{border-top-style:dotted}'
  };

  function rememberedAdv() {
    try {
      return window.localStorage.getItem(ADV_KEY) === '1';
    } catch (e) {
      return false;
    }
  }

  /** Shows or hides the second toolbar row, as WordPress's Toolbar Toggle (`wp_adv`). */
  function showAdv(editor, show) {
    editor.getContainer().classList.toggle('mc-editor-adv-hidden', !show);
  }

  /**
   * Inserts the Read More tag in a paragraph of its own after the top-level block the cursor
   * is in (a paragraph, list, blockquote or table), or in place of an empty top-level
   * paragraph. WordPress's wpmore inserts inline in a top-level paragraph and relies on
   * force_balance_tags when the post is split; Mission Control keeps the tag between blocks
   * (Ruling R131).
   */
  function insertMore(editor) {
    var dom = editor.dom;
    var body = editor.getBody();
    var block = editor.selection.getNode();
    while (block && block.parentNode && block.parentNode !== body) {
      block = block.parentNode;
    }
    if (!block || block === body || !block.parentNode) {
      editor.insertContent(placeholder('more', ''));
      return;
    }
    if (block.nodeName === 'P' && dom.isEmpty(block)) {
      editor.insertContent(placeholder('more', ''));
      return;
    }
    editor.undoManager.transact(function () {
      var line = dom.create('p', {}, placeholder('more', ''));
      dom.insertAfter(line, block);
      editor.selection.select(line.firstChild);
      editor.selection.collapse(false);
    });
    editor.nodeChanged();
  }

  function setup(editor) {
    editor.ui.registry.addButton('mcmore', {
      icon: 'page-break',
      tooltip: 'Insert Read More tag',
      onAction: function () {
        insertMore(editor);
      }
    });
    editor.ui.registry.addToggleButton('mcadv', {
      icon: 'more-drawer',
      tooltip: 'Toolbar Toggle',
      onAction: function (button) {
        var show = !button.isActive();
        button.setActive(show);
        showAdv(editor, show);
        try {
          window.localStorage.setItem(ADV_KEY, show ? '1' : '0');
        } catch (e) {
          // No storage: the second row is hidden at the next start.
        }
      },
      onSetup: function (button) {
        button.setActive(rememberedAdv());
        return function () {};
      }
    });
    editor.on('init', function () {
      showAdv(editor, rememberedAdv());
    });
    // WordPress's guard: a hidden Visual editor never writes over the Text tab.
    editor.on('SaveContent', function (event) {
      if (editor.isHidden() && event.element) {
        event.content = event.element.value;
      }
    });
    editor.on('BeforeSetContent', function (event) {
      if (event.format === 'raw' || typeof event.content !== 'string') {
        return;
      }
      if (event.load) {
        event.content = autop(event.content);
      }
      event.content = toPlaceholders(event.content);
    });
    editor.on('PostProcess', function (event) {
      if (event.get && typeof event.content === 'string') {
        event.content = fromPlaceholders(event.content);
      }
    });
  }

  function remember(mode) {
    try {
      window.localStorage.setItem(MODE_KEY, mode);
    } catch (e) {
      // No storage: the Visual tab is the default.
    }
  }

  function remembered() {
    try {
      return window.localStorage.getItem(MODE_KEY) === 'html' ? 'html' : 'tinymce';
    } catch (e) {
      return 'tinymce';
    }
  }

  /** The editors on this page, by textarea id. */
  var editors = {};

  function Editor(textarea) {
    this.textarea = textarea;
    this.id = textarea.id;
    this.instance = null;
    this.starting = null;
    // What the Visual tab loaded: the textarea's HTML, and TinyMCE's serialization of it.
    this.loaded = null;
    this.mode = 'html';
    this.build();
  }

  Editor.prototype.build = function () {
    var self = this;
    var textarea = this.textarea;
    var wrap = document.createElement('div');
    wrap.className = 'mc-editor';
    var tools = document.createElement('div');
    tools.className = 'mc-editor-tools';

    var media = document.createElement('button');
    media.type = 'button';
    media.className = 'ig-btn ig-btn-outline mc-editor-media';
    media.textContent = 'Add Media';
    media.setAttribute('data-editor', this.id);
    media.addEventListener('click', function () {
      self.addMedia();
    });

    var tabs = document.createElement('div');
    tabs.className = 'mc-editor-tabs';
    tabs.setAttribute('role', 'tablist');
    this.visualTab = this.tab('Visual', 'tinymce');
    this.textTab = this.tab('Text', 'html');
    tabs.appendChild(this.visualTab);
    tabs.appendChild(this.textTab);

    tools.appendChild(media);
    tools.appendChild(tabs);
    var container = document.createElement('div');
    container.className = 'mc-editor-container';
    textarea.parentNode.insertBefore(wrap, textarea);
    wrap.appendChild(tools);
    wrap.appendChild(container);
    container.appendChild(textarea);
    textarea.classList.add('mc-editor-area');
    this.wrap = wrap;

    if (textarea.form) {
      textarea.form.addEventListener('submit', function () {
        self.save();
      });
    }
  };

  Editor.prototype.tab = function (label, mode) {
    var self = this;
    var button = document.createElement('button');
    button.type = 'button';
    button.className = 'mc-editor-tab';
    button.textContent = label;
    button.setAttribute('role', 'tab');
    button.setAttribute('aria-controls', this.id);
    button.setAttribute('data-mode', mode);
    button.addEventListener('click', function () {
      self.switchTo(mode);
      remember(mode);
    });
    return button;
  };

  Editor.prototype.mark = function (mode) {
    this.mode = mode;
    this.wrap.classList.toggle('mc-editor-visual', mode === 'tinymce');
    this.wrap.classList.toggle('mc-editor-text', mode === 'html');
    this.visualTab.setAttribute('aria-selected', String(mode === 'tinymce'));
    this.textTab.setAttribute('aria-selected', String(mode === 'html'));
  };

  /**
   * Starts TinyMCE on the textarea, once. It is started only when the Visual tab is first
   * shown, so a page opened on the Text tab sends the textarea exactly as it was.
   */
  Editor.prototype.start = function () {
    var self = this;
    if (this.starting) {
      return this.starting;
    }
    this.starting = loadTinymce().then(function (tinymce) {
      var options = Object.assign({}, settings, {
        target: self.textarea,
        setup: setup
      });
      return tinymce.init(options);
    }).then(function (instances) {
      self.instance = instances[0] || null;
      if (self.instance) {
        self.keepLoaded();
      }
      return self.instance;
    });
    return this.starting;
  };

  Editor.prototype.switchTo = function (mode) {
    var self = this;
    if (mode === 'html') {
      if (this.instance && !this.instance.isHidden()) {
        this.hideVisual();
      }
      this.mark('html');
      return;
    }
    this.mark('tinymce');
    if (this.instance) {
      if (this.instance.isHidden()) {
        // `show` reads the textarea, so what was typed in the Text tab is kept.
        this.instance.show();
        this.keepLoaded();
        this.instance.undoManager.clear();
        // A base level, so the first change after the switch can be undone.
        this.instance.undoManager.add();
      }
      return;
    }
    this.start().then(function (instance) {
      if (instance && self.mode === 'html') {
        self.hideVisual();
      }
    }, function (error) {
      self.mark('html');
      if (window.console) {
        window.console.error('The Visual editor could not start', error);
      }
    });
  };

  /** Keeps what the Visual tab has just loaded from the textarea. */
  Editor.prototype.keepLoaded = function () {
    this.loaded = {
      source: this.textarea.value,
      serialized: this.instance.getContent()
    };
  };

  /**
   * The HTML the Visual tab holds, for the textarea: TinyMCE's serialization when it differs
   * from what was loaded, otherwise the HTML as it was loaded, so opening a post (or switching
   * tabs) without changing it does not rewrite its HTML. The comparison does not depend on
   * TinyMCE's dirty state, which some commands do not set.
   */
  Editor.prototype.visualHtml = function () {
    var html = this.instance.getContent();
    if (this.loaded && html === this.loaded.serialized) {
      return this.loaded.source;
    }
    return html;
  };

  /** Shows the Text tab, with the Visual tab's HTML in the textarea. */
  Editor.prototype.hideVisual = function () {
    var html = this.visualHtml();
    this.instance.hide();
    this.textarea.value = html;
  };

  /** Writes the Visual tab's HTML into the textarea before the form is sent. */
  Editor.prototype.save = function () {
    var instance = this.instance;
    if (instance && !instance.isHidden()) {
      this.textarea.value = this.visualHtml();
    }
  };

  Editor.prototype.insert = function (html) {
    var instance = this.instance;
    if (instance && !instance.isHidden()) {
      instance.focus();
      instance.insertContent(html);
      return;
    }
    var textarea = this.textarea;
    var start = textarea.selectionStart || 0;
    var end = textarea.selectionEnd || start;
    textarea.value = textarea.value.slice(0, start) + html + textarea.value.slice(end);
    textarea.selectionStart = textarea.selectionEnd = start + html.length;
    textarea.focus();
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
  };

  Editor.prototype.addMedia = function () {
    if (api.media && typeof api.media.open === 'function') {
      api.media.open(this.id);
      return;
    }
    insertFromUrl(this);
  };

  /**
   * The built-in Add Media dialog, until the media library offers its own (`mcEditor.media`):
   * WordPress's "Insert from URL", an image's address and its alternative text.
   */
  function insertFromUrl(editor) {
    var dialog = document.createElement('dialog');
    dialog.className = 'mc-editor-dialog';
    dialog.innerHTML =
      '<form method="dialog">' +
      '<h2>Insert from URL</h2>' +
      // A text field, not `type="url"`: the site's own paths (`/mc-content/uploads/...`) are
      // relative URLs, which `type="url"` refuses.
      '<label>URL <input class="ig-input" type="text" name="url" required></label>' +
      '<label>Alternative text <input class="ig-input" type="text" name="alt"></label>' +
      '<div class="mc-editor-actions">' +
      '<button class="ig-btn ig-btn-primary" value="insert">Insert into post</button>' +
      '<button class="ig-btn ig-btn-outline" value="cancel" formnovalidate>Cancel</button>' +
      '</div></form>';
    document.body.appendChild(dialog);
    dialog.addEventListener('close', function () {
      var form = dialog.querySelector('form');
      var url = form.elements.url.value.trim();
      if (dialog.returnValue === 'insert' && url) {
        editor.insert('<img src="' + escapeAttribute(url) + '" alt="' +
          escapeAttribute(form.elements.alt.value) + '" />');
      }
      dialog.remove();
    });
    dialog.showModal();
  }

  var tinymceLoading = null;

  /** Loads `tinymce.min.js` from the bundle, once. */
  function loadTinymce() {
    if (window.tinymce) {
      return Promise.resolve(window.tinymce);
    }
    if (tinymceLoading) {
      return tinymceLoading;
    }
    tinymceLoading = new Promise(function (resolve, reject) {
      var tag = document.createElement('script');
      tag.src = tinymceBase + '/tinymce.min.js' + query;
      tag.onload = function () {
        if (window.tinymce) {
          resolve(window.tinymce);
        } else {
          reject(new Error('tinymce.min.js did not define tinymce'));
        }
      };
      tag.onerror = function () {
        reject(new Error('tinymce.min.js could not be loaded'));
      };
      document.head.appendChild(tag);
    });
    return tinymceLoading;
  }

  function addStylesheet() {
    var link = document.createElement('link');
    link.rel = 'stylesheet';
    link.href = stylesheet;
    document.head.appendChild(link);
  }

  api.insert = function (editorId, html) {
    var editor = editors[editorId];
    if (editor) {
      editor.insert(html);
    }
  };

  api.isVisual = function (editorId) {
    var editor = editors[editorId];
    return Boolean(editor && editor.mode === 'tinymce');
  };

  function start() {
    var areas = document.querySelectorAll('textarea[data-ignite-editor="wysiwyg"]');
    if (!areas.length) {
      return;
    }
    addStylesheet();
    var mode = remembered();
    Array.prototype.forEach.call(areas, function (textarea) {
      if (!textarea.id || editors[textarea.id]) {
        return;
      }
      var editor = new Editor(textarea);
      editors[textarea.id] = editor;
      editor.mark('html');
      editor.switchTo(mode);
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
}());
