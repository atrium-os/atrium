"""Generate synthetic documents for the four-app Prelum IPPS gate."""
import struct
import sys
import zipfile
import zlib
from pathlib import Path
from xml.sax.saxutils import escape

def chunk(t,b):
    return struct.pack('>I',len(b))+t+b+struct.pack('>I',zlib.crc32(t+b))
def png(width, height, left, right):
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    row = b"\0" + left * (width // 2) + right * (width - width // 2)
    return (
        b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(row * height)) + chunk(b"IEND", b"")
    )


def sheets(output):
    dest = output / 'sheets-proof.xlsx'
    m='http://schemas.openxmlformats.org/spreadsheetml/2006/main'
    r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'
    p='http://schemas.openxmlformats.org/package/2006/relationships'
    d='http://schemas.openxmlformats.org/drawingml/2006'
    def rel(*parts):
        return '<Relationships xmlns="'+p+'">'+''.join(f'<Relationship Id="{i}" Type="{r}/{kind}" Target="{dst}"/>' for i,kind,dst in parts)+'</Relationships>'
    def corner(tag,col,row):
        return f'<xdr:{tag}><xdr:col>{col}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{row}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:{tag}>'
    def picture(col,row,w,h,ref="image"):
        return f'<xdr:oneCellAnchor>{corner("from",col,row)}<xdr:ext cx="{w*9525}" cy="{h*9525}"/><xdr:pic><xdr:blipFill><a:blip r:embed="{ref}"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:oneCellAnchor>'
    chart_anchor=f'<xdr:twoCellAnchor>{corner("from",2,4)}{corner("to",8,17)}<xdr:graphicFrame><a:graphic><a:graphicData><c:chart r:id="chart"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:twoCellAnchor>'
    chart=f'<c:chartSpace xmlns:c="{d}/chart" xmlns:a="{d}/main"><c:chart><c:title><c:tx><c:rich><a:p><a:r><a:t>Quarterly Chart Proof</a:t></a:r></a:p></c:rich></c:tx></c:title><c:plotArea><c:barChart><c:barDir val="col"/><c:grouping val="clustered"/><c:ser><c:cat><c:strRef><c:f>Data!$A$2:$A$4</c:f></c:strRef></c:cat><c:val><c:numRef><c:f>Data!$B$2:$B$4</c:f></c:numRef></c:val></c:ser></c:barChart></c:plotArea></c:chart></c:chartSpace>'
    with zipfile.ZipFile(dest,'w',zipfile.ZIP_DEFLATED) as z:
        z.writestr('[Content_Types].xml','<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="png" ContentType="image/png"/><Default Extension="jpg" ContentType="image/jpeg"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>')
        z.writestr('_rels/.rels',rel(('book','officeDocument','xl/workbook.xml')))
        z.writestr('xl/workbook.xml',f'<workbook xmlns="{m}" xmlns:r="{r}"><sheets><sheet name="Data" sheetId="1" r:id="data"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">Data!$A$1:$I$26</definedName></definedNames></workbook>')
        z.writestr('xl/_rels/workbook.xml.rels',rel(('data','worksheet','worksheets/sheet1.xml')))
        rows='<row r="1"><c r="A1" t="inlineStr"><is><t>Proof</t></is></c><c r="J1" t="inlineStr"><is><t>OUTRIGHT</t></is></c></row>'+''.join(f'<row r="{i}"><c r="A{i}" t="inlineStr"><is><t>{label}</t></is></c><c r="B{i}"><v>{val}</v></c></row>' for i,label,val in [(2,'North',4),(3,'South',7),(4,'West',5)])
        rows += '<row r="27"><c r="A27" t="inlineStr"><is><t>OUTBELOW</t></is></c></row>'
        z.writestr('xl/worksheets/sheet1.xml',f'<worksheet xmlns="{m}" xmlns:r="{r}"><sheetData>{rows}</sheetData><drawing r:id="d"/></worksheet>')
        z.writestr('xl/worksheets/_rels/sheet1.xml.rels',rel(('d','drawing','../drawings/drawing1.xml')))
        z.writestr('xl/drawings/drawing1.xml',f'<xdr:wsDr xmlns:xdr="{d}/spreadsheetDrawing" xmlns:a="{d}/main" xmlns:c="{d}/chart" xmlns:r="{r}">{chart_anchor}{picture(2,18,144,72)}{picture(5,18,96,72,"photo")}{picture(12,18,144,72,"outside")}</xdr:wsDr>')
        z.writestr('xl/drawings/_rels/drawing1.xml.rels',rel(('chart','chart','../charts/chart1.xml'),('image','image','../media/picture.png'),('photo','image','../media/photo.png'),('outside','image','../media/outside.png')))
        z.writestr('xl/charts/chart1.xml',chart)
        z.writestr("xl/media/picture.png", png(24, 18, bytes.fromhex("ff00dd"), bytes.fromhex("00cdad")))
        z.writestr("xl/media/photo.png", png(31, 19, bytes.fromhex("ff9900"), bytes.fromhex("2060ff")))
        z.writestr("xl/media/outside.png", png(17, 13, bytes.fromhex("111111"), bytes.fromhex("eeeeee")))



def slides(output):
    p='http://schemas.openxmlformats.org/presentationml/2006/main'
    a='http://schemas.openxmlformats.org/drawingml/2006/main'
    r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'
    pkg='http://schemas.openxmlformats.org/package/2006/relationships'
    ns=f'xmlns:p="{p}" xmlns:a="{a}" xmlns:r="{r}"'
    def rel(*links):
        return f'<Relationships xmlns="{pkg}">'+''.join(f'<Relationship Id="{id}" Type="{r}/{kind}" Target="{target}"/>' for id,kind,target in links)+'</Relationships>'
    def para(text, size):
        return f'<a:p><a:r><a:rPr lang="en-US" sz="{size}"/><a:t>{escape(text)}</a:t></a:r></a:p>'
    labels=['FIRST SLIDE','HIDDEN SLIDE','SECOND SLIDE','THIRD SLIDE','FOURTH SLIDE']
    notes=[[f'Speaker line {i:02d}' for i in range(1,91)]+['FINAL SPEAKER LINE'],['HIDDEN SPEAKER NOTE'],['SECOND SPEAKER NOTE'],[],['FOURTH SPEAKER NOTE']]
    with zipfile.ZipFile(output/'slides-proof.pptx','w',zipfile.ZIP_DEFLATED) as z:
        z.writestr('[Content_Types].xml','<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>')
        z.writestr('_rels/.rels',rel(('p','officeDocument','ppt/presentation.xml')))
        ids=''.join(f'<p:sldId id="{255+i}" r:id="s{i}"/>' for i in range(1,6))
        z.writestr('ppt/presentation.xml',f'<p:presentation {ns}><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/></p:presentation>')
        z.writestr('ppt/_rels/presentation.xml.rels',rel(*[(f's{i}','slide',f'slides/slide{i}.xml') for i in range(1,6)]))
        for i,(label,body) in enumerate(zip(labels, notes),1):
            show=' show="0"' if i==2 else ''
            color=['','E5F2FB','E00000','D8F9E5','FBF0C8','EAE0FD'][i]
            shape=f'<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="635000" y="635000"/><a:ext cx="10500000" cy="2400000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/>{para(label,4200)}</p:txBody></p:sp>'
            z.writestr(f'ppt/slides/slide{i}.xml',f'<p:sld {ns}{show}><p:cSld><p:bg><p:bgPr><a:solidFill><a:srgbClr val="{color}"/></a:solidFill><a:effectLst/></p:bgPr></p:bg><p:spTree>{shape}</p:spTree></p:cSld></p:sld>')
            if body:
                z.writestr(f'ppt/slides/_rels/slide{i}.xml.rels',rel(('note','notesSlide',f'../notesSlides/notesSlide{i}.xml')))
                tx=''.join(para(line,1200) for line in body)
                z.writestr(f'ppt/notesSlides/notesSlide{i}.xml',f'<p:notes {ns}><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Notes"/><p:cNvSpPr/><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{tx}</p:txBody></p:sp></p:spTree></p:cSld></p:notes>')

if __name__ == "__main__":
    output = Path(sys.argv[1])
    output.mkdir(parents=True, exist_ok=True)
    sheets(output)
    slides(output)
    paragraphs = "".join(f"<p>HTML fixture paragraph {n:03d}</p>" for n in range(1, 96))
    (output / "quicklook.html").write_text(
        "<!doctype html><title>Quick Look proof</title><style>h1{color:#1653ab}</style>"
        "<h1>HTML FIRST HEADING</h1>" + paragraphs + "<h2>HTML FINAL HEADING</h2>"
    )
    lines = ["FIRST QUICK LOOK LINE"]
    lines.extend(f"Print fixture line {n:03d}" for n in range(1, 121))
    lines.append("FINAL QUICK LOOK LINE")
    (output / "quicklook-proof.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    (output / "quicklook-picture.png").write_bytes(
        png(120, 60, bytes.fromhex("ff0033"), bytes.fromhex("0044ff"))
    )
