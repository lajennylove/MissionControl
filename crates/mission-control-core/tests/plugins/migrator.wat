;; A sample plugin built from `wit/mission-control.wit` (world `plugin-with-migrations`),
;; written in the component model's text format (Ruling R99). Permission: `migrations`.
;;
;; A template: the test puts the plugin's migrations, as JSON text, in the data segment at
;; 65536 and their length in `migrations` before it compiles the component (see
;; `tests/plugin_migrations.rs`). Its `migrations` export returns that text; its handler does
;; nothing.
(component
  ;; The memory and the allocator the host copies strings in with: the heap starts at 128 KiB,
  ;; past the JSON.
  (core module $libc
    (memory (export "memory") 3)
    (global $heap (mut i32) (i32.const 131072))
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

  (core module $main
    (import "libc" "memory" (memory 1))
    ;; 65536: the migrations, as JSON text.
    (data (i32.const 65536) "@JSON@")
    ;; 512: where exports write their results.

    (func (export "init") (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "activate") (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "deactivate") (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "on-action") (param i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "apply-filter") (param i32 i32 i32 i32 i32 i32) (result i32)
      unreachable)

    (func (export "migrations") (result i32)
      (i32.store (i32.const 512) (i32.const 65536))
      (i32.store (i32.const 516) (i32.const @LEN@))
      (i32.const 512))
  )
  (core instance $main (instantiate $main (with "libc" (instance $libc))))

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
  (func $migrations (result string)
    (canon lift (core func $main "migrations") (memory $mem) (realloc $realloc)))
  (instance $handler
    (export "init" (func $init))
    (export "activate" (func $activate))
    (export "deactivate" (func $deactivate))
    (export "on-action" (func $on-action))
    (export "apply-filter" (func $apply-filter))
  )
  (instance $migrations
    (export "migrations" (func $migrations))
  )
  (export "mission-control:plugin/handler@0.1.0" (instance $handler))
  (export "mission-control:plugin/migrations@0.1.0" (instance $migrations))
)
