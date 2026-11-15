;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). Permissions: `hooks`, `read-content` and `write-content`.
;;
;; On activation it subscribes to the action `mc_test_edit`. When the action fires with a post
;; id, it reads the post (get-post), sets its title to "<b>Changed</b>" (update-post), and
;; copies the post meta `source`, when the post has it, to the post meta `copy` (get-post-meta,
;; set-post-meta). The first call the host refuses fails the handler with "<call> failed".
(component
  (type $hooks-type (instance
    (export "add-action" (func (param "name" string) (param "priority" s32) (result (result (error string)))))
  ))
  (import "mission-control:plugin/hooks@0.1.0" (instance $hooks (type $hooks-type)))

  (type $content-read-type (instance
    (type $error (variant
      (case "forbidden" string)
      (case "not-found" string)
      (case "invalid" string)
      (case "failed" string)))
    (export "error" (type $error-export (eq $error)))
    (type $post (record
      (field "id" s64)
      (field "post-type" string)
      (field "status" string)
      (field "title" string)
      (field "slug" string)
      (field "content" string)
      (field "excerpt" string)
      (field "author-id" s64)
      (field "parent-id" (option s64))
      (field "menu-order" s32)
      (field "published-at" (option string))
      (field "modified-at" string)
      (field "password-protected" bool)))
    (export "post" (type $post-export (eq $post)))
    (export "get-post" (func (param "id" s64) (result (result $post-export (error $error-export)))))
    (export "get-post-meta" (func (param "post-id" s64) (param "key" string)
      (result (result (option string) (error $error-export)))))
  ))
  (import "mission-control:plugin/content-read@0.1.0" (instance $content-read (type $content-read-type)))

  (type $content-write-type (instance
    (type $error (variant
      (case "forbidden" string)
      (case "not-found" string)
      (case "invalid" string)
      (case "failed" string)))
    (export "error" (type $error-export (eq $error)))
    (type $changes (record
      (field "title" (option string))
      (field "content" (option string))
      (field "excerpt" (option string))))
    (export "post-changes" (type $changes-export (eq $changes)))
    (export "update-post" (func (param "id" s64) (param "changes" $changes-export)
      (result (result (error $error-export)))))
    (export "set-post-meta" (func (param "post-id" s64) (param "key" string) (param "value-json" string)
      (result (result (error $error-export)))))
  ))
  (import "mission-control:plugin/content-write@0.1.0" (instance $content-write (type $content-write-type)))

  ;; The memory and the allocator the host copies strings in with.
  (core module $libc
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 4096))
    (func (export "cabi_realloc") (param i32 i32) (param $align i32) (param $size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (global.set $heap (i32.add (local.get $ptr) (local.get $size)))
      (if (i32.gt_u (global.get $heap) (i32.mul (memory.size) (i32.const 65536)))
        (then
          (if (i32.eq
                (memory.grow
                  (i32.shr_u
                    (i32.add
                      (i32.sub (global.get $heap) (i32.mul (memory.size) (i32.const 65536)))
                      (i32.const 65535))
                    (i32.const 16)))
                (i32.const -1))
            (then unreachable))))
      (local.get $ptr))
  )
  (core instance $libc (instantiate $libc))
  (alias core export $libc "memory" (core memory $mem))
  (alias core export $libc "cabi_realloc" (core func $realloc))

  (core func $add-action (canon lower (func $hooks "add-action") (memory $mem) (realloc $realloc)))
  (core func $get-post (canon lower (func $content-read "get-post") (memory $mem) (realloc $realloc)))
  (core func $get-post-meta
    (canon lower (func $content-read "get-post-meta") (memory $mem) (realloc $realloc)))
  (core func $update-post
    (canon lower (func $content-write "update-post") (memory $mem) (realloc $realloc)))
  (core func $set-post-meta
    (canon lower (func $content-write "set-post-meta") (memory $mem) (realloc $realloc)))

  (core module $main
    (import "libc" "memory" (memory 1))
    (import "hooks" "add-action" (func $add-action (param i32 i32 i32 i32)))
    (import "content-read" "get-post" (func $get-post (param i64 i32)))
    (import "content-read" "get-post-meta" (func $get-post-meta (param i64 i32 i32 i32)))
    (import "content-write" "update-post"
      (func $update-post (param i64 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)))
    (import "content-write" "set-post-meta" (func $set-post-meta (param i64 i32 i32 i32 i32 i32)))
    (data (i32.const 16) "mc_test_edit")
    (data (i32.const 32) "<b>Changed</b>")
    (data (i32.const 48) "source")
    (data (i32.const 56) "copy")
    (data (i32.const 64) "get-post failed")
    (data (i32.const 80) "update-post failed")
    (data (i32.const 112) "get-post-meta failed")
    (data (i32.const 144) "set-post-meta failed")
    ;; 256: where imports write small results; 512: where exports write theirs;
    ;; 1024 to 2047: where get-post writes the post.

    (func $fail (param $message i32) (param $len i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 1))
      (i32.store (i32.const 516) (local.get $message))
      (i32.store (i32.const 520) (local.get $len))
      (i32.const 512))

    (func (export "activate") (result i32)
      (call $add-action (i32.const 16) (i32.const 12) (i32.const 10) (i32.const 256))
      (i32.const 256))

    ;; The payload is a post id in decimal.
    (func (export "on-action") (param $name i32) (param $name-len i32)
      (param $payload i32) (param $payload-len i32) (result i32)
      (local $id i64) (local $i i32) (local $digit i32)
      (block $done
        (loop $next
          (br_if $done (i32.ge_u (local.get $i) (local.get $payload-len)))
          (local.set $digit
            (i32.sub (i32.load8_u (i32.add (local.get $payload) (local.get $i))) (i32.const 48)))
          (br_if $done (i32.gt_u (local.get $digit) (i32.const 9)))
          (local.set $id
            (i64.add (i64.mul (local.get $id) (i64.const 10)) (i64.extend_i32_u (local.get $digit))))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $next)))

      (call $get-post (local.get $id) (i32.const 1024))
      (if (i32.load8_u (i32.const 1024))
        (then (return (call $fail (i32.const 64) (i32.const 15)))))

      ;; title: some("<b>Changed</b>"); content and excerpt: none.
      (call $update-post (local.get $id)
        (i32.const 1) (i32.const 32) (i32.const 14)
        (i32.const 0) (i32.const 0) (i32.const 0)
        (i32.const 0) (i32.const 0) (i32.const 0)
        (i32.const 256))
      (if (i32.load8_u (i32.const 256))
        (then (return (call $fail (i32.const 80) (i32.const 18)))))

      (call $get-post-meta (local.get $id) (i32.const 48) (i32.const 6) (i32.const 256))
      (if (i32.load8_u (i32.const 256))
        (then (return (call $fail (i32.const 112) (i32.const 20)))))
      ;; When the option is some, copy the value.
      (if (i32.load8_u (i32.const 260))
        (then
          (call $set-post-meta (local.get $id) (i32.const 56) (i32.const 4)
            (i32.load (i32.const 264)) (i32.load (i32.const 268)) (i32.const 256))
          (if (i32.load8_u (i32.const 256))
            (then (return (call $fail (i32.const 144) (i32.const 20)))))))

      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    ;; Subscribes to no filter: gives the value back.
    (func (export "apply-filter") (param i32 i32) (param $value i32) (param $value-len i32)
      (param i32 i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.store (i32.const 516) (local.get $value))
      (i32.store (i32.const 520) (local.get $value-len))
      (i32.const 512))
  )
  (core instance $main (instantiate $main
    (with "libc" (instance $libc))
    (with "hooks" (instance (export "add-action" (func $add-action))))
    (with "content-read" (instance
      (export "get-post" (func $get-post))
      (export "get-post-meta" (func $get-post-meta))))
    (with "content-write" (instance
      (export "update-post" (func $update-post))
      (export "set-post-meta" (func $set-post-meta))))
  ))

  (func $activate (result (result (error string)))
    (canon lift (core func $main "activate") (memory $mem) (realloc $realloc)))
  (func $on-action (param "name" string) (param "payload-json" string) (result (result (error string)))
    (canon lift (core func $main "on-action") (memory $mem) (realloc $realloc)))
  (func $apply-filter (param "name" string) (param "value-json" string) (param "args-json" string)
    (result (result string (error string)))
    (canon lift (core func $main "apply-filter") (memory $mem) (realloc $realloc)))
  (instance $handler
    (export "activate" (func $activate))
    (export "on-action" (func $on-action))
    (export "apply-filter" (func $apply-filter))
  )
  (export "mission-control:plugin/handler@0.1.0" (instance $handler))
)
