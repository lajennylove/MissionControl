import { cpSync, existsSync } from 'node:fs';
import { networkInterfaces } from 'node:os';
import { basename, dirname } from 'node:path';
import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';
import laravel from 'laravel-vite-plugin';

// The theme's slug is its folder name, as in WordPress. Mission Control serves the theme's
// build at /themes/<slug>/build/, so built CSS must point there for its fonts and images.
const slug = basename(dirname(fileURLToPath(import.meta.url)));

// The fonts theme.json bundles (resources/fonts) and the theme's images are linked by their
// path, so they are copied as they are. laravel-vite-plugin does not copy images and fonts into the build, so
// `asset('resources/images/x.svg')` would have nothing to point at. Copy them
// into public/build/resources, next to the hashed files.
function copyStaticAssets() {
    return {
        name: 'propel:copy-static-assets',
        apply: 'build',
        closeBundle() {
            for (const dir of ['images', 'fonts']) {
                const source = `resources/${dir}`;
                if (existsSync(source)) {
                    cpSync(source, `public/build/resources/${dir}`, { recursive: true });
                }
            }
        },
    };
}

const alias = (path) => fileURLToPath(new URL(path, import.meta.url));

// The dev server URL that Propel reads from public/hot. Vite's default would be
// localhost, which a phone on the same network cannot reach, so use this
// machine's LAN address. It is read when `npm run dev` starts, so restart the
// dev server after the address changes.
function lanOrigin(port = 5173) {
    for (const addresses of Object.values(networkInterfaces())) {
        for (const address of addresses ?? []) {
            if (address.family === 'IPv4' && !address.internal) {
                return `http://${address.address}:${port}`;
            }
        }
    }
    return undefined;
}

export default defineConfig(({ command }) => ({
    // Built files are served under the theme's folder, not the site's /build/.
    base: command === 'build' ? `/themes/${slug}/build/` : undefined,
    plugins: [
        laravel({
            input: ['resources/css/app.css', 'resources/js/app.js'],
            // Views are read at runtime and are not part of the Vite graph, so a
            // view edit reloads the page instead of rebuilding.
            refresh: ['*.propel', 'layouts/**/*.propel', 'partials/**/*.propel'],
        }),
        copyStaticAssets(),
    ],
    server: {
        host: true,
        origin: lanOrigin(),
    },
    resolve: {
        alias: {
            '@scripts': alias('./resources/js'),
            '@styles': alias('./resources/css'),
            '@fonts': alias('./resources/fonts'),
            '@images': alias('./resources/images'),
        },
    },
}));
