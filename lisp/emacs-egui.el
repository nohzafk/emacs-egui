;;; emacs-egui.el --- Generic GPU-accelerated egui host framework -*- lexical-binding: t; -*-

;; Author: emacs-egui
;; Version: 0.1.0
;; Keywords: convenience, frames, tools, wasm, egui
;; Package-Requires: ((emacs "29.1"))

;;; Commentary:
;; A generic framework for running high-performance GPU-accelerated egui
;; interfaces inside Emacs. It spins up a local TCP socket HTTP server to host
;; WebAssembly assets, streams local files to the sandboxed WebKit frame,
;; and manages bidirectional IPC between Lisp and Rust.

;;; Code:

(require 'cl-lib)
(require 'xwidget)
(require 'json)
(require 'url-util)

(defgroup emacs-egui nil
  "Generic GPU-accelerated egui host framework."
  :group 'tools
  :prefix "emacs-egui-")

(defvar emacs-egui--httpd-process nil
  "The global background TCP server process.")

(defvar emacs-egui--httpd-port nil
  "The ephemeral local port the server is bound to.")

(defvar emacs-egui--httpd-url nil
  "The base URL of the local asset server.")

(defvar emacs-egui--callbacks (make-hash-table :test 'equal)
  "Hash table mapping session-id string to callback functions.
Keys are \"session-id:action\", value is callback function.")

(defvar emacs-egui--sessions (make-hash-table :test 'equal)
  "Hash table of active session metadata.")

;; ---------------------------------------------------------------------------
;; Local Asset Server & Secure File Gateway
;; ---------------------------------------------------------------------------

(defvar emacs-egui--dir
  (file-name-directory (or load-file-name buffer-file-name))
  "Directory of this file, used to resolve application relative paths.")

(defun emacs-egui--get-app-dir (app-name)
  "Locate the base directory of a registered app by name."
  ;; Assumes standard folder structure: /Users/randall/projects/<app-name>/renderer/
  (expand-file-name (format "../../%s/renderer/" app-name)
                    emacs-egui--dir))

(defun emacs-egui--content-type (file)
  "Return Content-Type based on extension for FILE."
  (pcase (downcase (or (file-name-extension file) ""))
    ("html" "text/html; charset=utf-8")
    ("js"   "text/javascript; charset=utf-8")
    ("wasm" "application/wasm")
    ("json" "application/json; charset=utf-8")
    ("css"  "text/css; charset=utf-8")
    (_      "application/octet-stream")))

(defun emacs-egui--read-file-bytes (file)
  "Return the raw bytes of FILE as a unibyte string."
  (with-temp-buffer
    (set-buffer-multibyte nil)
    (insert-file-contents-literally file)
    (buffer-string)))

(defun emacs-egui--httpd-send (proc status ctype body)
  "Send HTTP response over PROC.  BODY must be a unibyte string."
  (when (process-live-p proc)
    (let ((header (encode-coding-string
                   (format (concat "HTTP/1.1 %s\r\n"
                                   "Content-Type: %s\r\n"
                                   "Content-Length: %d\r\n"
                                   "Access-Control-Allow-Origin: *\r\n"
                                   "Cache-Control: no-store\r\n"
                                   "Connection: close\r\n\r\n")
                           status ctype (length body))
                   'utf-8)))
      (process-send-string proc (concat header body))
      (process-send-eof proc))))

(defun emacs-egui--resolve-asset (app-name path)
  "Resolve relative HTTP request PATH for APP-NAME to a local file, or nil."
  (let ((app-dir (emacs-egui--get-app-dir app-name)))
    (when (and app-dir (file-directory-p app-dir))
      (let* ((clean (car (split-string path "[?#]")))
             (rel (if (member clean '("/" "")) "index.html"
                    (string-remove-prefix "/" clean)))
             (base (file-name-as-directory (expand-file-name app-dir)))
             (file (expand-file-name rel base)))
        (and (string-prefix-p base file)  ; reject path traversal
             (file-readable-p file)
             (not (file-directory-p file))
             file)))))

(defun emacs-egui--hex-decode (str)
  "Decode a hex-encoded string STR."
  (let ((res ""))
    (cl-loop for i from 0 to (- (length str) 2) by 2 do
             (setq res (concat res (char-to-string (string-to-number (substring str i (+ i 2)) 16)))))
    res))

(defun emacs-egui--httpd-respond (proc raw-path)
  "Route and respond to request RAW-PATH over PROC."
  (cond
   ;; 1. The binary file gateway: /api/file?path=<encoded_path>
   ((string-prefix-p "/api/file" raw-path)
    (let* ((query (cadr (split-string raw-path "\\?")))
           (params (and query (url-parse-query-string query)))
           (path-encoded (car (assoc-default "path" params)))
           (path (and path-encoded (url-unhex-string path-encoded))))
      (if (and path (file-readable-p path) (not (file-directory-p path)))
          (progn
            (message "emacs-egui: streaming file %s" path)
            (emacs-egui--httpd-send proc "200 OK" "application/octet-stream"
                                   (emacs-egui--read-file-bytes path)))
        (emacs-egui--httpd-send proc "404 Not Found" "text/plain; charset=utf-8"
                                (string-to-unibyte "File Not Found")))))

   ;; 2. Inbound event callbacks: /api/event?session=<session-id>&action=<action>&payload=<url_encoded_json>
   ((string-prefix-p "/api/event" raw-path)
    (let* ((query (cadr (split-string raw-path "\\?")))
           (params (and query (url-parse-query-string query)))
           (session-id (car (assoc-default "session" params)))
           (action (car (assoc-default "action" params)))
           (payload-json (car (assoc-default "payload" params)))
           (payload (and payload-json (json-read-from-string (url-unhex-string payload-json)))))
      (if (and session-id action)
          (let ((cb (gethash (format "%s:%s" session-id action) emacs-egui--callbacks)))
            (if cb
                (progn
                  (run-at-time 0 nil cb payload)
                  (emacs-egui--httpd-send proc "200 OK" "application/json; charset=utf-8"
                                          (string-to-unibyte "{\"status\":\"ok\"}")))
              (emacs-egui--httpd-send proc "404 Not Found" "text/plain; charset=utf-8"
                                      (string-to-unibyte "Callback Not Registered"))))
        (emacs-egui--httpd-send proc "400 Bad Request" "text/plain; charset=utf-8"
                                (string-to-unibyte "Missing Parameters")))))

   ;; 3. Serve application static assets: /app/<app-name>/<file>
   ((string-prefix-p "/app/" raw-path)
    (let* ((parts (split-string (string-remove-prefix "/app/" raw-path) "/"))
           (app-name (car parts))
           (asset-path (concat "/" (mapconcat #'identity (cdr parts) "/")))
           (file (emacs-egui--resolve-asset app-name asset-path)))
      (if file
          (emacs-egui--httpd-send proc "200 OK"
                                  (emacs-egui--content-type file)
                                  (emacs-egui--read-file-bytes file))
        (emacs-egui--httpd-send proc "404 Not Found" "text/plain; charset=utf-8"
                                (string-to-unibyte "Asset Not Found")))))

   (t
    (emacs-egui--httpd-send proc "404 Not Found" "text/plain; charset=utf-8"
                            (string-to-unibyte "Not Found")))))

(defun emacs-egui--httpd-filter (proc chunk)
  "Filter to parse TCP chunk and dispatch responder."
  (let ((buf (concat (process-get proc :emacs-egui-request) chunk)))
    (process-put proc :emacs-egui-request buf)
    (when (string-match "\r\n\r\n" buf)
      (let* ((request-line (car (split-string buf "\r\n")))
             (fields (split-string request-line " "))
             (method (nth 0 fields))
             (path (or (nth 1 fields) "/")))
        (process-put proc :emacs-egui-request "")
        (if (member method '("GET" "HEAD"))
            (emacs-egui--httpd-respond proc path)
          (emacs-egui--httpd-send proc "405 Method Not Allowed"
                                  "text/plain; charset=utf-8"
                                  (string-to-unibyte "Method Not Allowed")))))))

(defun emacs-egui-ensure-server ()
  "Start the global TCP HTTP asset server if not already running."
  (unless (and emacs-egui--httpd-process
               (process-live-p emacs-egui--httpd-process))
    (setq emacs-egui--httpd-process
          (make-network-process
           :name "emacs-egui-httpd"
           :server t
           :host 'local
           :service t
           :family 'ipv4
           :coding 'binary
           :filter #'emacs-egui--httpd-filter
           :noquery t))
    (setq emacs-egui--httpd-port
          (process-contact emacs-egui--httpd-process :service))
    (setq emacs-egui--httpd-url
          (format "http://127.0.0.1:%s" emacs-egui--httpd-port))
    (message "emacs-egui: server running at %s" emacs-egui--httpd-url))
  emacs-egui--httpd-url)

;; ---------------------------------------------------------------------------
;; Custom Theme Sync
;; ---------------------------------------------------------------------------

(defun emacs-egui--theme-payload ()
  "Return active theme color data for standard default face."
  (let* ((bg (face-background 'default nil 'default))
         (fg (face-foreground 'default nil 'default))
         (height (face-attribute 'default :height nil 'default))
         (font-size
          (cond
           ((integerp height) (/ height 10.0))
           ((floatp height) (* height 12.0))
           (t 12.0))))
    (list :bg bg
          :fg fg
          :font-size font-size
          :surface-bg bg)))

(defun emacs-egui--url-with-theme (app-name session-id)
  "Build the boot URL for APP-NAME incorporating theme parameters."
  (let* ((theme (emacs-egui--theme-payload))
         (bg (plist-get theme :bg))
         (fg (plist-get theme :fg))
         (font-size (plist-get theme :font-size)))
    (format "%s/app/%s/index.html#bg=%s&fg=%s&font-size=%s&session=%s&port=%d"
            emacs-egui--httpd-url
            app-name
            (url-hexify-string bg)
            (url-hexify-string fg)
            (url-hexify-string (format "%s" font-size))
            session-id
            emacs-egui--httpd-port)))

;; ---------------------------------------------------------------------------
;; IPC: Push Lisp -> WASM
;; ---------------------------------------------------------------------------

(defun emacs-egui-send-state (session state)
  "Push STATE (alist or plist) as JSON to the active SESSION."
  (let ((xwidget (plist-get session :xwidget)))
    (when (and xwidget (xwidget-live-p xwidget))
      (let* ((json-str (json-encode state))
             (script (format "if (window.egui_push_state) { window.egui_push_state(%S); } else if (window.eguiPushState) { window.eguiPushState(%S); }"
                             json-str json-str)))
        (xwidget-webkit-execute-script xwidget script)))))

(defun emacs-egui-send-theme (session)
  "Push active Emacs default face colors to the active SESSION."
  (let ((xwidget (plist-get session :xwidget)))
    (when (and xwidget (xwidget-live-p xwidget))
      (let* ((json-str (json-encode (emacs-egui--theme-payload)))
             (script (format "if (window.egui_push_theme) { window.egui_push_theme(%S); } else if (window.eguiPushTheme) { window.eguiPushTheme(%S); }"
                             json-str json-str)))
        (xwidget-webkit-execute-script xwidget script)))))

;; ---------------------------------------------------------------------------
;; Session & Buffer Lifecycle
;; ---------------------------------------------------------------------------

(defun emacs-egui-on (session action callback)
  "Register CALLBACK for a given ACTION triggered inside the active SESSION."
  (let ((session-id (plist-get session :id)))
    (puthash (format "%s:%s" session-id action) callback emacs-egui--callbacks)))

(cl-defun emacs-egui-create-buffer (&key app-name buffer-name)
  "Initialize HTTP server and instantiate xwidget inside a dedicated buffer.
Returns a plist holding the session context."
  (unless (featurep 'xwidget-internal)
    (error "emacs-egui: this Emacs is not built with xwidget support"))
  (emacs-egui-ensure-server)
  
  (let* ((session-id (format "%s-%s" app-name (random 100000)))
         (boot-url (emacs-egui--url-with-theme app-name session-id)))
    
    (let* ((orig-config (current-window-configuration))
           (_ (xwidget-webkit-new-session boot-url))
           (xwidget (xwidget-webkit-current-session))
           (buf (xwidget-buffer xwidget)))
      ;; Restore original window configuration so we don't disrupt current view
      (set-window-configuration orig-config)
      
      (with-current-buffer buf
        (rename-buffer buffer-name t)
        (setq-local mode-line-format nil)
        (setq-local header-line-format nil)
        (setq-local display-line-numbers nil)
        (setq-local left-fringe-width 0)
        (setq-local right-fringe-width 0))
      
      (let ((session (list :id session-id
                           :buffer buf
                           :xwidget xwidget
                           :app-name app-name)))
        (puthash session-id session emacs-egui--sessions)
        
        ;; Trigger layout/theme sync shortly after loading
        (run-with-timer 0.6 nil
                        (lambda ()
                          (emacs-egui-send-theme session)))
        session))))

(defun emacs-egui-shutdown ()
  "Kill the server process and clear all sessions."
  (interactive)
  (clrhash emacs-egui--callbacks)
  (clrhash emacs-egui--sessions)
  (when (and emacs-egui--httpd-process
             (process-live-p emacs-egui--httpd-process))
    (delete-process emacs-egui--httpd-process))
  (setq emacs-egui--httpd-process nil
        emacs-egui--httpd-port nil
        emacs-egui--httpd-url nil)
  (message "emacs-egui: server shut down."))

(provide 'emacs-egui)
;;; emacs-egui.el ends here
