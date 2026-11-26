/**
 * Mission Control's taxonomy boxes on the post editor's sidebar, as WordPress's classic editor
 * shows them. `editor.js` loads this file when the form has a taxonomy's field for new terms.
 *
 * - A hierarchical taxonomy (`tax_<taxonomy>`, with `new_<taxonomy>`): the category checklist,
 *   a tree of checkboxes (the choices are listed in tree order, a child's label indented by three
 *   no-break spaces per level), with "+ Add New Category" at the bottom. A category added there
 *   shows checked at the top of the list and is created by the save.
 * - A flat taxonomy (`tax_<taxonomy>`, with `tax_input_<taxonomy>`): the tags box, a text box
 *   where tags are typed separated by commas, with the existing tags suggested, and the post's
 *   tags below it, each with a button that removes it. A typed tag that does not exist yet is
 *   created by the save.
 *
 * The form still sends Ignite's own fields: the multiple select of the terms' ids (hidden here,
 * the boxes select its options) and the text field of the new terms' names (hidden, the boxes
 * fill it). Nothing changes until the user does, so the panel's unsaved changes alert stays
 * quiet on an unchanged form. Without this script the form shows those two fields as they are.
 */
(function () {
  'use strict';

  var api = window.mcEditor = window.mcEditor || {};
  if (api.termBoxes) {
    return;
  }
  api.termBoxes = true;

  /** The indent of one level of the category tree (`wp_dropdown_categories`'s `&nbsp;` x 3). */
  var INDENT = '   ';

  function fieldOf(name) {
    return document.querySelector('[data-ignite-field="' + name + '"]');
  }

  /** Tells the form (and the panel's alert) that `element`'s value changed. */
  function changed(element) {
    element.dispatchEvent(new Event('input', { bubbles: true }));
    element.dispatchEvent(new Event('change', { bubbles: true }));
  }

  /** The names in `text`, separated by commas: trimmed, without blanks. */
  function names(text) {
    return text.split(',').map(function (name) {
      return name.trim();
    }).filter(function (name) {
      return name !== '';
    });
  }

  /** The option of `select` whose label (without its indent) is `name`, compared without case. */
  function optionNamed(select, name) {
    var wanted = name.toLowerCase();
    return Array.prototype.find.call(select.options, function (option) {
      return option.textContent.replace(/^ +/, '').toLowerCase() === wanted;
    }) || null;
  }

  /** The names kept in the new terms' text field. */
  function newNames(input) {
    return names(input.value);
  }

  function setNewNames(input, list) {
    input.value = list.join(', ');
    changed(input);
  }

  function button(text, className) {
    var element = document.createElement('button');
    element.type = 'button';
    element.className = className;
    element.textContent = text;
    return element;
  }

  // -------------------------------------------------------------------------------------------
  // The category checklist

  function checkbox(label, checked, onChange) {
    var item = document.createElement('li');
    var wrap = document.createElement('label');
    var box = document.createElement('input');
    box.type = 'checkbox';
    box.checked = checked;
    box.addEventListener('change', function () {
      onChange(box.checked);
    });
    wrap.appendChild(box);
    wrap.appendChild(document.createTextNode(' ' + label));
    item.appendChild(wrap);
    return item;
  }

  function categoryBox(newWrap) {
    var taxonomy = newWrap.getAttribute('data-ignite-field').slice('new_'.length);
    var field = fieldOf('tax_' + taxonomy);
    var select = field && field.querySelector('select[multiple]');
    var input = newWrap.querySelector('input');
    if (!select || !input) {
      return;
    }
    var box = document.createElement('div');
    box.className = 'mc-terms mc-terms-categories';
    var list = document.createElement('ul');
    list.className = 'mc-terms-checklist';

    // The tree: each option after its parent, indented by its depth.
    var parents = [{ depth: -1, item: null, list: list }];
    Array.prototype.forEach.call(select.options, function (option) {
      var label = option.textContent;
      var depth = 0;
      while (label.indexOf(INDENT) === 0) {
        depth += 1;
        label = label.slice(INDENT.length);
      }
      while (parents.length > 1 && parents[parents.length - 1].depth >= depth) {
        parents.pop();
      }
      var parent = parents[parents.length - 1];
      if (!parent.list) {
        parent.list = document.createElement('ul');
        parent.item.appendChild(parent.list);
      }
      var item = checkbox(label, option.selected, function (checked) {
        option.selected = checked;
        changed(select);
      });
      parent.list.appendChild(item);
      parents.push({ depth: depth, item: item, list: null });
    });

    // The categories added here, checked at the top, until the save creates them.
    newNames(input).forEach(function (name) {
      list.insertBefore(newCategory(name), list.firstChild);
    });

    function newCategory(name) {
      return checkbox(name, true, function (checked) {
        var kept = newNames(input).filter(function (other) {
          return other.toLowerCase() !== name.toLowerCase();
        });
        if (checked) {
          kept.push(name);
        }
        setNewNames(input, kept);
      });
    }

    var toggle = button('+ Add New Category', 'mc-terms-toggle');
    toggle.setAttribute('aria-expanded', 'false');
    var form = document.createElement('div');
    form.className = 'mc-terms-add';
    form.hidden = true;
    var text = document.createElement('input');
    text.type = 'text';
    text.className = 'ig-input';
    text.setAttribute('aria-label', 'New Category Name');
    var add = button('Add New Category', 'ig-btn ig-btn-outline');
    form.appendChild(text);
    form.appendChild(add);

    toggle.addEventListener('click', function () {
      form.hidden = !form.hidden;
      toggle.setAttribute('aria-expanded', String(!form.hidden));
      if (!form.hidden) {
        text.focus();
      }
    });

    function addCategory() {
      var name = text.value.trim();
      if (!name) {
        return;
      }
      text.value = '';
      var existing = optionNamed(select, name);
      if (existing) {
        // As WordPress's check: an existing category is checked, not added again.
        existing.selected = true;
        changed(select);
        var index = Array.prototype.indexOf.call(select.options, existing);
        var boxes = list.querySelectorAll('input[type="checkbox"]');
        var offset = boxes.length - select.options.length;
        if (boxes[index + offset]) {
          boxes[index + offset].checked = true;
        }
        return;
      }
      var kept = newNames(input);
      if (kept.some(function (other) { return other.toLowerCase() === name.toLowerCase(); })) {
        return;
      }
      kept.push(name);
      setNewNames(input, kept);
      list.insertBefore(newCategory(name), list.firstChild);
    }

    add.addEventListener('click', addCategory);
    text.addEventListener('keydown', function (event) {
      if (event.key === 'Enter') {
        event.preventDefault();
        addCategory();
      }
    });

    box.appendChild(list);
    box.appendChild(toggle);
    box.appendChild(form);
    select.hidden = true;
    select.parentNode.insertBefore(box, select.nextSibling);
    newWrap.hidden = true;
  }

  // -------------------------------------------------------------------------------------------
  // The tags box

  function tagsBox(newWrap) {
    var taxonomy = newWrap.getAttribute('data-ignite-field').slice('tax_input_'.length);
    var field = fieldOf('tax_' + taxonomy);
    var select = field && field.querySelector('select[multiple]');
    var input = newWrap.querySelector('input');
    if (!select || !input) {
      return;
    }
    var box = document.createElement('div');
    box.className = 'mc-terms mc-terms-tags';

    var suggestions = document.createElement('datalist');
    suggestions.id = 'mc-terms-suggest-' + taxonomy;
    Array.prototype.forEach.call(select.options, function (option) {
      var suggestion = document.createElement('option');
      suggestion.value = option.textContent;
      suggestions.appendChild(suggestion);
    });

    var row = document.createElement('div');
    row.className = 'mc-terms-add';
    var text = document.createElement('input');
    text.type = 'text';
    text.className = 'ig-input';
    text.autocomplete = 'off';
    text.setAttribute('list', suggestions.id);
    text.setAttribute('aria-label', 'Add New Tag');
    var add = button('Add', 'ig-btn ig-btn-outline');
    row.appendChild(text);
    row.appendChild(add);

    var hint = document.createElement('p');
    hint.className = 'mc-terms-hint';
    hint.textContent = 'Separate tags with commas';

    var chips = document.createElement('ul');
    chips.className = 'mc-terms-chips';

    function chip(label, onRemove) {
      var item = document.createElement('li');
      var remove = button('×', 'mc-terms-remove');
      remove.setAttribute('aria-label', 'Remove term: ' + label);
      remove.addEventListener('click', onRemove);
      item.appendChild(remove);
      item.appendChild(document.createTextNode(' ' + label));
      return item;
    }

    function render() {
      chips.textContent = '';
      Array.prototype.forEach.call(select.options, function (option) {
        if (option.selected) {
          chips.appendChild(chip(option.textContent, function () {
            option.selected = false;
            changed(select);
            render();
          }));
        }
      });
      newNames(input).forEach(function (name) {
        chips.appendChild(chip(name, function () {
          setNewNames(input, newNames(input).filter(function (other) {
            return other.toLowerCase() !== name.toLowerCase();
          }));
          render();
        }));
      });
    }

    function addTags() {
      var typed = names(text.value);
      text.value = '';
      if (!typed.length) {
        return;
      }
      var kept = newNames(input);
      var selected = false;
      typed.forEach(function (name) {
        var existing = optionNamed(select, name);
        if (existing) {
          if (!existing.selected) {
            existing.selected = true;
            selected = true;
          }
        } else if (!kept.some(function (other) {
          return other.toLowerCase() === name.toLowerCase();
        })) {
          kept.push(name);
        }
      });
      if (selected) {
        changed(select);
      }
      if (kept.join(', ') !== input.value) {
        setNewNames(input, kept);
      }
      render();
    }

    add.addEventListener('click', addTags);
    text.addEventListener('keydown', function (event) {
      if (event.key === 'Enter') {
        event.preventDefault();
        addTags();
      }
    });

    box.appendChild(row);
    box.appendChild(suggestions);
    box.appendChild(hint);
    box.appendChild(chips);
    select.hidden = true;
    select.parentNode.insertBefore(box, select.nextSibling);
    newWrap.hidden = true;
    render();
  }

  function start() {
    document.querySelectorAll('[data-ignite-field^="new_"]').forEach(categoryBox);
    document.querySelectorAll('[data-ignite-field^="tax_input_"]').forEach(tagsBox);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
}());
