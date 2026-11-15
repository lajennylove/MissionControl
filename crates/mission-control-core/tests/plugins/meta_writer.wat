;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). Permissions: `hooks` and `write-content`; it also logs.
;;
;; On init it subscribes to the action `mc_test_saved`. When the action fires with a post
;; id, it sets the post meta `plugin_saw` of that post to the JSON string "seen" and logs
;; "handled mc_test_saved"; when the host refuses the write, the handler fails with
;; "set-post-meta failed".
(component
  (type $logging-type (instance
    (type $level (enum "error" "warn" "info" "debug" "trace"))
    (export "level" (type $level-export (eq $level)))
    (export "log" (func (param "level" $level-export) (param "message" string)))
  ))
  (import "mission-control:plugin/logging@0.1.0" (instance $logging (type $logging-type)))

  (type $hooks-type (instance
    (export "add-action" (func (param "name" string) (param "priority" s32) (result (result (error string)))))
  ))
  (import "mission-control:plugin/hooks@0.1.0" (instance $hooks (type $hooks-type)))

  (type $content-write-type (instance
    (type $error (variant
      (case "forbidden" string)
      (case "not-found" string)
      (case "invalid" string)
      (case "failed" string)))
    (export "error" (type $error-export (eq $error)))
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

  (core func $log (canon lower (func $logging "log") (memory $mem) (realloc $realloc)))
  (core func $add-action (canon lower (func $hooks "add-action") (memory $mem) (realloc $realloc)))
  (core func $set-post-meta
    (canon lower (func $content-write "set-post-meta") (memory $mem) (realloc $realloc)))

  (core module $main
    (import "libc" "memory" (memory 1))
    (import "logging" "log" (func $log (param i32 i32 i32)))
    (import "hooks" "add-action" (func $add-action (param i32 i32 i32 i32)))
    (import "content-write" "set-post-meta" (func $set-post-meta (param i64 i32 i32 i32 i32 i32)))
    ;; 16: the action (13 bytes); 32: the meta key (10); 48: the meta value (6);
    ;; 64: the log message (21); 96: the error (20).
    (data (i32.const 16) "mc_test_saved")
    (data (i32.const 32) "plugin_saw")
    (data (i32.const 48) "\22seen\22")
    (data (i32.const 64) "handled mc_test_saved")
    (data (i32.const 96) "set-post-meta failed")
    ;; 256: where imports write their results; 512: where exports write theirs.

    (func (export "init") (result i32)
      (call $add-action (i32.const 16) (i32.const 13) (i32.const 10) (i32.const 256))
      ;; add-action's `result<_, string>` is laid out as init's: pass it on.
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
      (call $set-post-meta (local.get $id) (i32.const 32) (i32.const 10) (i32.const 48) (i32.const 6)
        (i32.const 256))
      (if (i32.load8_u (i32.const 256))
        (then
          (i32.store8 (i32.const 512) (i32.const 1))
          (i32.store (i32.const 516) (i32.const 96))
          (i32.store (i32.const 520) (i32.const 20))
          (return (i32.const 512))))
      ;; Level 2 is `info`.
      (call $log (i32.const 2) (i32.const 64) (i32.const 21))
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    ;; Subscribes to no filter: gives the value back.
    (func (export "apply-filter") (param i32 i32) (param $value i32) (param $value-len i32)
      (param i32 i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.store (i32.const 516) (local.get $value))
      (i32.store (i32.const 520) (local.get $value-len))
      (i32.const 512))

    ;; Ruling R105: `activate` and `deactivate` do nothing; 8: where they write their result.
    (func (export "activate") (result i32)
      (i32.store8 (i32.const 8) (i32.const 0))
      (i32.const 8))

    (func (export "deactivate") (result i32)
      (i32.store8 (i32.const 8) (i32.const 0))
      (i32.const 8))
  )
  (core instance $main (instantiate $main
    (with "libc" (instance $libc))
    (with "logging" (instance (export "log" (func $log))))
    (with "hooks" (instance (export "add-action" (func $add-action))))
    (with "content-write" (instance (export "set-post-meta" (func $set-post-meta))))
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
