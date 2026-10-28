"use strict";

window.addEventListener("DOMContentLoaded", function() {
  const SUPPORTED_TYPES = ["Files", "application/x-moz-file"];

  let body = document.getElementsByTagName("body")[0];
  let file_upload_text = null;
  let remaining_files = 0;
  let pending_discovery = 0;
  let had_error = false;
  let had_upload = false;
  let url = document.location.pathname;
  if(!url.endsWith("/"))
    url += "/";

  body.addEventListener("dragover", function(ev) {
    if(SUPPORTED_TYPES.find(function(el) {
      return (ev.dataTransfer.types.contains || ev.dataTransfer.types.includes).call(ev.dataTransfer.types, el);
    }))
      ev.preventDefault();
  });

  body.addEventListener("drop", function(ev) {
    if(SUPPORTED_TYPES.find(function(el) {
      return (ev.dataTransfer.types.contains || ev.dataTransfer.types.includes).call(ev.dataTransfer.types, el);
    })) {
      ev.preventDefault();
      process_transfer(ev.dataTransfer);
    }
  });

  body.addEventListener("paste", function(ev) {
    if(ev.target.tagName != "INPUT" && (ev.clipboardData || window.clipboardData).files.length) {
      ev.preventDefault();
      process_transfer(ev.clipboardData || window.clipboardData);
    }
  });

  function process_transfer(transfer) {
    ++pending_discovery;
    try {
      const items = Array.from(transfer.items || []).filter(item => item.kind === "file");
      if(items.length) {
        for(const item of items) {
          try {
            const entry = item.webkitGetAsEntry ? item.webkitGetAsEntry() : null;
            if(entry)
              recurse_upload(entry, url);
            else {
              const file = item.getAsFile ? item.getAsFile() : null;
              if(!file)
                throw new Error("No file available from the transfer item");
              upload_file(url + encodeURIComponent(file.name), file);
            }
          } catch(error) {
            discovery_error("a dropped file", error);
          }
        }
      } else {
        for(const file of Array.from(transfer.files || []))
          upload_file(url + encodeURIComponent(file.name), file);
      }
    } finally {
      --pending_discovery;
      maybe_reload();
    }
  }

  function maybe_reload() {
    if(had_upload && !had_error && !remaining_files && !pending_discovery)
      window.location.reload();
  }

  function discovery_error(name, error) {
    had_error = true;
    let line = document.createElement("dd");
    line.innerText = "Unable to read " + name + ": " + (error.message || error);
    upload_list.appendChild(line);
  }

  let file_upload = document.querySelector("input[type=file]");
  let upload_list = document.createElement("dl");
  file_upload.parentElement.appendChild(upload_list);

  file_upload.addEventListener("change", function() {
    for(let i = file_upload.files.length - 1; i >= 0; --i) {
      let file = file_upload.files[i];
      upload_file(url + encodeURIComponent(file.name), file);
    }
  });

  function upload_file(req_url, file) {
    ++remaining_files;
    had_upload = true;

    let filename_line = document.createElement("dt");
    filename_line.innerText = file.name;
    upload_list.appendChild(filename_line)

    let progress_progress_label = document.createElement("label");
    let progress_progress_desc  = document.createElement("span");
    let progress_progress       = document.createElement("progress");
    progress_progress.value = 0;
    progress_progress_label.appendChild(progress_progress);
    progress_progress_label.appendChild(document.createTextNode(" "));
    progress_progress_desc.innerText = "pending";
    progress_progress_label.appendChild(progress_progress_desc);

    let progress_speed = document.createElement("span");
    let progress_speed_text = document.createTextNode("?");
    progress_speed.appendChild(progress_speed_text);
    progress_speed.appendChild(document.createTextNode("/s"));

    let progress_time = document.createElement("span");
    progress_speed.appendChild(progress_time);

    let progress_line     = document.createElement("dd");
    progress_line.appendChild(progress_progress_label);
    progress_line.appendChild(document.createTextNode(" "));
    progress_line.appendChild(progress_speed);
    progress_line.appendChild(document.createTextNode(" "));
    progress_line.appendChild(progress_time);
    upload_list.appendChild(progress_line)

    if(!file_upload_text) {
      file_upload_text = document.createTextNode(1);
      file_upload.parentNode.insertBefore(file_upload_text, file_upload.nextSibling); // insertafter
    } else
      file_upload_text.data = remaining_files;

    let request = new XMLHttpRequest();
    request.addEventListener("loadend", function(e) {
      --remaining_files;
      if(request.status >= 200 && request.status < 300) {
        filename_line.remove();
        progress_line.remove();
      } else {
        had_error = true;
        progress_line.innerText = request.response;
        file_upload.disabled = true;
      }
      file_upload_text.data = remaining_files;
      maybe_reload();
    });

    let start = 0;
    request.upload.addEventListener("loadstart", function(e) {
      start = e.timeStamp;
    });
    let prog = function(e) {
      if(e.lengthComputable) {
        progress_progress.value = e.loaded;
        progress_progress.max   = e.total;

        let elapsed = (e.timeStamp - start) / 1000; // s
        if(elapsed > 0.1) {
          let speed = e.loaded / elapsed;
          progress_progress_desc.innerText = human_readable_size(e.loaded) + "/" + human_readable_size(e.total);
          progress_speed_text.data         = human_readable_size(speed);
          progress_time.innerText          = hms(elapsed) + "/" + hms(e.total / speed);
        }
      } else {
        progress_progress.removeAttribute("value");
        progress_progress_desc.innerText = "uploading";
        request.upload.removeEventListener("progress", prog);
      }
    };
    request.upload.addEventListener("progress", prog);

    request.open("PUT", req_url);
    if(file.lastModified)
      request.setRequestHeader("X-Last-Modified", file.lastModified);
    request.send(file);
  }

  function recurse_upload(entry, base_url) {
    ++pending_discovery;
    let settled = false;
    function finish(error) {
      if(settled)
        return;
      settled = true;
      if(error)
        discovery_error(entry ? entry.fullPath : "entry", error);
      --pending_discovery;
      maybe_reload();
    }
    try {
      if(!entry)
        throw new Error("No filesystem entry available");
      if(entry.isFile) {
        function ready(file) {
          upload_file(base_url + entry.fullPath.split("/").filter(function(seg) { return seg; }).map(encodeURIComponent).join("/"), file);
          finish();
        }
        if(entry.file)
          entry.file(ready, finish);
        else
          Promise.resolve(entry.getFile()).then(ready, finish);
      } else
        all_in_reader(entry.createReader(), function(child) {
          recurse_upload(child, base_url);
        }, finish);
    } catch(error) {
      finish(error);
    }
  }

  function all_in_reader(reader, f, finish) {
    try {
      reader.readEntries(function(entries) {
        entries.forEach(f);
        if(entries.length)
          all_in_reader(reader, f, finish);
        else
          finish();
      }, finish);
    } catch(error) {
      finish(error);
    }
  }

  // Ported from util::HumanReadableSize
  const LN_KIB = Math.LN2 * 10; // 1024f64.ln()
  function human_readable_size(num) {
    let exp = Math.min(Math.max(Math.trunc(Math.log(num) / LN_KIB), 0), 8);

    let val = num / Math.pow(2, exp * 10);

    return ((exp > 0) ? Math.round(val * 10) / 10 : Math.round(val)) + " "
           + ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"][Math.trunc(Math.max(exp, 0))];
  }

  function hms(seconds) {
    let h = Math.trunc(seconds / 3600);
    let m = Math.trunc((seconds % 3600) / 60);
    let s = Math.trunc(seconds % 60);

    if(h)
      return h + ":" + String(m).padStart(2, '0') + ":" + String(s).padStart(2, '0');
    else if(m)
      return m + ":" + String(s).padStart(2, '0');
    else
      return seconds.toFixed(1) + "s";
  }
});
