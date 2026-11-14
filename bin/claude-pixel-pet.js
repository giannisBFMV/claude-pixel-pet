#!/usr/bin/env node
// Starts the pet in the background and returns to the prompt; the pet keeps running after this exits.
const { spawn } = require("child_process");
const path = require("path");

const exe = path.join(__dirname, "..", "target", "release", "pixel_pet.exe");
spawn(exe, process.argv.slice(2), { detached: true, stdio: "ignore" }).unref();
console.log("Your pixel pet is on the desktop (bottom right). Drag it anywhere; right-click it to quit.");
