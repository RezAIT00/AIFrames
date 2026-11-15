' Runs a PowerShell script with no console window flash.
'   wscript run-hidden.vbs <script.ps1> [args...]
Dim args, i
args = ""
For i = 1 To WScript.Arguments.Count - 1
    args = args & " """ & WScript.Arguments(i) & """"
Next
CreateObject("WScript.Shell").Run "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File """ & WScript.Arguments(0) & """" & args, 0, False
