const output = document.querySelector("#output");

document.querySelector("#hello").onclick = async () => {
  try {
    const result = await window.bt.call("hello", {
      name: "BT Language"
    });

    output.textContent = JSON.stringify(result, null, 2);
  } catch (error) {
    output.textContent = "bt.call failed: " + error;
  }
};

document.querySelector("#title").onclick = async () => {
  await window.bt.window.set_title("Title Changed");
  output.textContent = "Window title changed";
};

document.querySelector("#size").onclick = async () => {
  await window.bt.window.set_size(1000, 700);
  output.textContent = "Window resized to 1000x700";
};

let resizable = true;
document.querySelector("#resizable").onclick = async () => {
  resizable = !resizable;
  await window.bt.window.set_resizable(resizable);
  output.textContent = "Window resizing: " + (resizable ? "on" : "off");
};

let fullscreen = false;
document.querySelector("#fullscreen").onclick = async () => {
  fullscreen = !fullscreen;
  await window.bt.window.set_fullscreen(fullscreen);
  output.textContent = "Window fullscreen: " + (fullscreen ? "on" : "off");
};
