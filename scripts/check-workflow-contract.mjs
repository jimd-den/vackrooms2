#!/usr/bin/env node

/**
 * @file check-workflow-contract.mjs
 * @description Whitepaper & Validation Test for GitHub Actions Workflow Specifications.
 *
 * BUSINESS LOGIC & ARCHITECTURAL PURPOSE:
 * 1. Environment Protection Compliance:
 *    GitHub Pages deployments use the protected `github-pages` environment. Environment
 *    protection rules configured on GitHub restrict deployments to the `master` branch.
 *    When workflows run on feature branches (e.g. `feature/low-spec-greedy-surface`), attempting
 *    to execute the `deploy` job results in deployment rejection errors.
 *    To satisfy compliance and prevent build failures on feature branches, the `deploy` job in
 *    `.github/workflows/pages.yml` MUST be gated with a conditional guard (`if: github.ref == 'refs/heads/master'`).
 *
 * 2. Node.js Toolchain Standardization:
 *    The build and test steps execute Node.js automation scripts (`scripts/build-single-file.mjs`,
 *    `scripts/build-wasm.mjs`, `scripts/check-wasm-fresh.mjs`). To ensure reproducible builds and
 *    leverage the latest Node features and security patches, both `pages.yml` and `test.yml` MUST
 *    explicitly configure the Node.js environment using `actions/setup-node@v4` set to `node-version: 'latest'`.
 */

import { readFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

/**
 * Log helper following observability guidelines with ISO 8601 timestamps.
 * @param {string} functionName - Name of the calling function.
 * @param {object} inputArgs - Inputs provided to the function.
 * @param {string} message - Telemetry message.
 */
function logTelemetry(functionName, inputArgs, message) {
  const timestamp = new Date().toISOString();
  console.log(`[${timestamp}] [${functionName}] Input: ${JSON.stringify(inputArgs)} | ${message}`);
}

async function verifyWorkflowContract() {
  const functionName = 'verifyWorkflowContract';
  const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const pagesYmlPath = join(rootDir, '.github', 'workflows', 'pages.yml');
  const testYmlPath = join(rootDir, '.github', 'workflows', 'test.yml');

  logTelemetry(functionName, { pagesYmlPath, testYmlPath }, 'Starting workflow verification contract checks.');

  const pagesYmlContent = await readFile(pagesYmlPath, 'utf8');
  const testYmlContent = await readFile(testYmlPath, 'utf8');

  const errors = [];

  // Check 1: pages.yml must set up Node.js with version 'latest'
  if (!pagesYmlContent.includes('actions/setup-node@v4') || !pagesYmlContent.includes("node-version: 'latest'")) {
    errors.push('pages.yml missing `actions/setup-node@v4` with `node-version: \'latest\'`');
  }

  // Check 2: pages.yml deploy job must be conditional on master branch
  if (!pagesYmlContent.includes("if: github.ref == 'refs/heads/master'")) {
    errors.push('pages.yml deploy job missing `if: github.ref == \'refs/heads/master\'` guard');
  }

  // Check 3: test.yml wasm job must set up Node.js with version 'latest'
  if (!testYmlContent.includes('actions/setup-node@v4') || !testYmlContent.includes("node-version: 'latest'")) {
    errors.push('test.yml missing `actions/setup-node@v4` with `node-version: \'latest\'`');
  }

  if (errors.length > 0) {
    logTelemetry(functionName, { errorsCount: errors.length }, `Verification failed with ${errors.length} error(s).`);
    for (const err of errors) {
      console.error(`- RED ASSERTION FAILURE: ${err}`);
    }
    process.exit(1);
  }

  logTelemetry(functionName, { status: 'SUCCESS' }, 'All workflow contracts satisfied successfully (GREEN).');
}

verifyWorkflowContract().catch((err) => {
  console.error('Fatal error during contract check:', err);
  process.exit(1);
});
