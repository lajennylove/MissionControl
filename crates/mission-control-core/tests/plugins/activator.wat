;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). Permission: `write-content`.
;;
;; Its `init` does nothing. Its `activate` sets the post meta `mc_activated` of post 1 to the
;; JSON `true`, as the acting user; when the host refuses, activation fails with "set-post-meta
;; failed". Its `deactivate` deletes that meta (Ruling R105: WordPress's activation and
;; deactivation hooks).
(component
  (type $content-write-type (instance
    (type $error (variant
      (case "forbidden" string)
      (case "not-found" string)
      (case "invalid" string)
      (case "failed" string)))
    (export "error" (type $error-export (eq $error)))
    (export "set-post-meta" (func (param "post-id" s64) (param "key" string) (param "value-json" string)
      (result (result (error $error-export)))))
    (export "delete-post-meta" (func (param "post-id" s64) (param "key" string)
      (result (result bool (error $error-export)))))
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

  (core func $set-post-meta
    (canon lower (func $content-write "set-post-meta") (memory $mem) (realloc $realloc)))
  (core func $delete-post-meta
    (canon lower (func $content-write "delete-post-meta") (memory $mem) (realloc $realloc)))

  (core module $main
    (import "libc" "memory" (memory 1))
    (import "content-write" "set-post-meta" (func $set-post-meta (param i64 i32 i32 i32 i32 i32)))
    (import "content-write" "delete-post-meta" (func $delete-post-meta (param i64 i32 i32 i32)))
    ;; 16: the meta key (12 bytes); 32: the value (4); 48: the error (20).
    (data (i32.const 16) "mc_activated")
    (data (i32.const 32) "true")
    (data (i32.const 48) "set-post-meta failed")
    ;; 256: where imports write their results; 512: where exports write theirs.

    (func (export "init") (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "activate") (result i32)
      (call $set-post-meta (i64.const 1) (i32.const 16) (i32.const 12) (i32.const 32) (i32.const 4)
        (i32.const 256))
      (if (i32.load8_u (i32.const 256))
        (then
          (i32.store8 (i32.const 512) (i32.const 1))
          (i32.store (i32.const 516) (i32.const 48))
          (i32.store (i32.const 520) (i32.const 20))
          (return (i32.const 512))))
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "deactivate") (result i32)
      (call $delete-post-meta (i64.const 1) (i32.const 16) (i32.const 12) (i32.const 256))
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "on-action") (param i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "apply-filter") (param i32 i32 i32 i32 i32 i32) (result i32)
      unreachable)
  )
  (core instance $main (instantiate $main
    (with "libc" (instance $libc))
    (with "content-write" (instance
      (export "set-post-meta" (func $set-post-meta))
      (export "delete-post-meta" (func $delete-post-meta))))
  ))

  (func $init (result (result (error string)))
    (canon lift (core func $main "init") (memory $mem) (realloc $realloc)))
  (func $activate (result (result (error string)))
    (canon lift (core func $main "activate") (memory $mem) (realloc $realloc)))
  (func $deactivate (result (result (error string)))
    (canon lift (core func $main "deactivate") (memory $mem) (realloc $realloc)))
  (func $on-action (param "name" string) (param "payload-json" string) (result (result (error string)))
    (canon lift (core func $main "on-action") (memory $mem) (realloc $realloc)))
  (func $apply-filter (param "name" string) (param "value-json" string) (param "args-json" string)
    (result (result string (error string)))
    (canon lift (core func $main "apply-filter") (memory $mem) (realloc $realloc)))
  (instance $handler
    (export "init" (func $init))
    (export "activate" (func $activate))
    (export "deactivate" (func $deactivate))
    (export "on-action" (func $on-action))
    (export "apply-filter" (func $apply-filter))
  )
  (export "mission-control:plugin/handler@0.1.0" (instance $handler))
)
