;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). Permission: `read-content`.
;;
;; Its `activate` reads post 1 (get-post) and logs "queried", forever: the fuel each content
;; call costs stops it.
(component
  (type $logging-type (instance
    (type $level (enum "error" "warn" "info" "debug" "trace"))
    (export "level" (type $level-export (eq $level)))
    (export "log" (func (param "level" $level-export) (param "message" string)))
  ))
  (import "mission-control:plugin/logging@0.1.0" (instance $logging (type $logging-type)))
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
  ))
  (import "mission-control:plugin/content-read@0.1.0" (instance $content-read (type $content-read-type)))

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

  (core func $log (canon lower (func $logging "log") (memory $mem) (realloc $realloc)))
  (core func $get-post (canon lower (func $content-read "get-post") (memory $mem) (realloc $realloc)))

  (core module $main
    (import "libc" "memory" (memory 1))
    (import "logging" "log" (func $log (param i32 i32 i32)))
    (import "content-read" "get-post" (func $get-post (param i64 i32)))
    ;; 16: the message (7 bytes); 1024 to 2047: where get-post writes the post.
    (data (i32.const 16) "queried")

    (func (export "activate") (result i32)
      (loop $forever
        (call $get-post (i64.const 1) (i32.const 1024))
        ;; Level 2 is `info`.
        (call $log (i32.const 2) (i32.const 16) (i32.const 7))
        (br $forever))
      unreachable)

    (func (export "on-action") (param i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "apply-filter") (param i32 i32 i32 i32 i32 i32) (result i32)
      unreachable)
  )
  (core instance $main (instantiate $main
    (with "libc" (instance $libc))
    (with "logging" (instance (export "log" (func $log))))
    (with "content-read" (instance (export "get-post" (func $get-post))))
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
