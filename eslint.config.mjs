// Flat config (ESLint 9 / eslint-config-next 16 — `next lint` foi removido no
// Next 16; o script `lint` chama o eslint CLI direto). Migração 1:1 do antigo
// .eslintrc.json.
import { defineConfig, globalIgnores } from "eslint/config";
import nextPlugin from "@next/eslint-plugin-next";
import reactHooks from "eslint-plugin-react-hooks";
import tseslint from "typescript-eslint";

export default defineConfig([
  // `.claude/worktrees/` são checkouts locais de outros agentes (com `.next/` e
  // `node_modules/` próprios) — nunca fonte deste repo; lintá-los explode o eslint
  // com dezenas de milhares de falsos positivos em JS gerado. (Na CI, checkout
  // limpo, o diretório nem existe.)
  // Cópias compiladas da demonstração e binários baixados pelo Playwright
  // vivem no scratch local. Os scripts escritos à mão em .superpowers seguem
  // sob lint; somente estes dois tipos de artefato gerado ficam de fora.
  // O `cf-crm/` é Rust: compilá-lo deixa JS GERADO no disco — o `build/` do
  // `worker-build` (sempre) e o `.wrangler/tmp` do `wrangler dev` (enquanto
  // roda). O ESLint não lê o `.gitignore`, e medido em 29/09/2026 esse JS dava
  // 37 erros de `no-unused-expressions` em quem só tinha rodado o `cargo`. Na
  // CI, checkout limpo, nenhum dos três existe.
  globalIgnores([".next/", "node_modules/", "dist/", "supabase/", "next-env.d.ts", ".claude/worktrees/", ".superpowers/**/bundles/", ".superpowers/**/playwright-browsers/", "cf-crm/target/", "cf-crm/worker/build/", "**/.wrangler/"]),
  nextPlugin.configs["core-web-vitals"],
  reactHooks.configs.flat.recommended,
  ...tseslint.configs.recommended,
  {
    rules: {
      "@typescript-eslint/no-unused-vars": [
        "warn",
        { argsIgnorePattern: "^_", varsIgnorePattern: "^_" },
      ],
      "@typescript-eslint/consistent-type-imports": ["warn", { prefer: "type-imports" }],
      "no-console": ["warn", { allow: ["warn", "error", "info"] }],
      // react-hooks 7 introduziu esta regra como error; o padrão setState-em-
      // effect é pré-existente em 14 componentes — warn até o mutirão de refactor.
      "react-hooks/set-state-in-effect": "warn",
    },
  },
  {
    // Script CLI do gov-loop (roda via tsx, fora do bundle) — require() ok.
    files: ["loop/**/*.ts"],
    rules: { "@typescript-eslint/no-require-imports": "off" },
  },
]);
